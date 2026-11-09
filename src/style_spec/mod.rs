//! Compiles a Mapbox/MapLibre style-spec JSON document into this crate's
//! own `crate::style::Style` (see the submodules for the three pieces:
//! `color` for CSS color strings, `filter` for `"filter"` expressions,
//! `expression` for zoom-driven paint-property expressions).
//!
//! Scope: the `background`/`fill`/`line`/`symbol` layer types -- plus
//! `fill-extrusion`, which compiles to `StyleLayer::Extrusion` from
//! `EXTRUSION_MIN_ZOOM` up and to a flat `fill` of its footprint below that
//! (see `compile_extrusion_cases`, and
//! `FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED` for the Mapbox-only
//! properties it drops) -- and the paint
//! properties this crate's renderer can actually draw
//! (`background-color`, `fill-color`, `fill-opacity`, `line-color`,
//! `line-width`, `line-opacity`, `line-gap-width`, `line-dasharray`,
//! plus `symbol`'s
//! `text-field`/`icon-image`/
//! `text-size`/`text-offset`/`text-anchor`/`text-max-width`/`text-color`/
//! `text-halo-color`/
//! `text-halo-width`/`symbol-sort-key`/`symbol-placement`/`symbol-spacing`/
//! `icon-rotate`/`icon-rotation-alignment`/`text-rotation-alignment`/
//! `icon-allow-overlap`/`icon-ignore-placement` -- constant or zoom-stepped
//! `"point"`/`"line"` placement, see `compile_symbol_layer`).
//!
//! Three different things can happen to a construct this module doesn't
//! handle, and they are NOT the same:
//!
//! - **Out of scope -> silently skipped, no error, sometimes not even a
//!   notice.** Every layer type other than `background`/`fill`/`line`/
//!   `symbol`/`fill-extrusion` (`raster`, `model`, `circle`, ...) is
//!   skipped with a
//!   one-line stderr notice, since a real-world style is expected to
//!   contain plenty of them. `fill-extrusion` used to be on that list and
//!   is now a FOURTH outcome, the same shape as `line-dasharray`'s silent
//!   degradation but at whole-layer scale: the layer is kept and drawn
//!   flat, and every 3D-only property (height, base, walls, flood light,
//!   ambient occlusion -- the full audited list is in
//!   `FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED`) vanishes with NO
//!   notice, because unlike the properties two paragraphs down it is not an
//!   oversight that they are unread. Skipping the type outright was NOT the
//!   conservative choice it looked like: styles use `fill-extrusion` to
//!   REPLACE a flat `fill` above a zoom threshold, so the skip deleted
//!   every building from the map at exactly the zoom a user is looking at
//!   buildings. A `fill` layer with no `fill-color` (i.e. a
//!   `fill-pattern`-only fill, e.g. Liberty's `landcover_wetland` and
//!   `road_area_pattern`) is likewise skipped with a stderr notice rather
//!   than treated as an error. Paint properties this module never reads on
//!   an otherwise in-scope, successfully-compiled layer vanish with NO
//!   notice at all -- verified against the vendored
//!   `examples/liberty-style.json`, these are:
//!   `fill-translate-anchor` (1). `line-gap-width` (1 there, and 23 of the
//!   134 layers of the vendored Mapbox Streets document -- Mapbox's entire
//!   road-casing system) used to be on this list and is now read, in the
//!   `"line"` arm of `compile_layer`, cross-producted with the other line
//!   paint properties exactly as `line-width` is. `fill-outline-color` (2) and
//!   `fill-antialias` (4) used to be on this list; both are now read, in
//!   the `"fill"` arm of `compile_layer` -- the outline colour as a full
//!   data-driven/zoom expression cross-producted like the other fill
//!   properties, the antialias flag as the literal gate that switches the
//!   outline off (MapLibre's `drawOutline` does the same). The spec's
//!   "`fill-outline-color` defaults to `fill-color`" is deliberately NOT
//!   reproduced: see `style::StyleLayer::Fill`'s `outline_color` for why an
//!   outline the exact colour of the fill under it is skipped rather than
//!   baked. `layout.visibility` used to be on this
//!   list too; it is now read (in `compile_single_layer`, before the layer
//!   type is even dispatched) -- folded as an EXPRESSION in `scope`, not
//!   just read as a literal string, because Mapbox Standard gates whole
//!   layer categories with e.g. `["case", ["config", "showPlaceLabels"],
//!   "visible", "none"]` -- and a layer whose folded visibility is
//!   `"none"` is dropped whole -- SILENTLY, since that is the style
//!   author (or its config) turning the layer off on purpose, not a
//!   construct this renderer failed to support (see the comment there). A
//!   `visibility` expression the folder cannot reduce to a constant is the
//!   one case that is NOT silent: unlike a paint property, there is no
//!   safe literal to degrade an on/off switch to, so it is hidden with a
//!   loud notice instead -- see the comment there for why hiding, not
//!   showing, is the safer guess. `line-dasharray` (17 occurrences
//!   there) used to be on this list and is now compiled; a dasharray this
//!   crate can't represent is a *third* thing again -- neither skip nor
//!   error, but a silent degradation to a solid line, so that an
//!   unsupported dash pattern can never cost the map a whole road class
//!   (see the `"line"` arm's `line-dasharray` block). `or_degrade`
//!   generalizes that third outcome to every compiled paint value: an
//!   expression the constant folder (`expression::eval`) recognized and
//!   could not reduce to a value degrades the PROPERTY to a literal and
//!   keeps the layer, with a notice naming the layer and the property.
//!   Only the folder's own refusals take this path -- an expression shape
//!   this module does not implement at all is still the loud per-layer skip
//!   below.
//! - **In scope but this one layer's filter/paint compile fails -> loud
//!   per-layer skip, not a whole-style failure.** An in-scope
//!   `background`/`fill`/`line`/`symbol` layer whose `"filter"` or a paint
//!   property (`compile_filter`/`compile_zoom_value`, or for `symbol`,
//!   `compile_text_field`/`compile_icon_image`/`compile_layout_value`)
//!   fails to compile because it uses some construct outside the supported
//!   set (e.g. a filter operator, or an expression shape, this module
//!   doesn't recognize -- Mapbox Streets-v12's `landcover` layer, with a
//!   zoom-dependent `match` filter, is a real example) is skipped with a
//!   stderr notice naming the layer id and the error, and the rest of the
//!   style keeps compiling. This is the same call as the two "out of scope"
//!   skips above, just discovered later (mid-compile instead of up front):
//!   one layer using a construct beyond this module's Phase-1 scope
//!   shouldn't block every other, perfectly-compilable layer in the style
//!   from loading. `symbol` layers get one extra case of this: both
//!   constant and zoom-stepped `"point"`/`"line"`/`"line-center"`
//!   `symbol-placement` compile, but a keyword none of them recognizes is
//!   skipped the same way.
//! - **Structurally broken -> hard error, whole style fails.** A JSON shape
//!   problem that isn't "an unsupported construct" but "this document
//!   doesn't even describe a layer" -- the top-level JSON failing to parse,
//!   no top-level `"layers"` array, a layer with no `"type"`, an in-scope
//!   layer missing a required field (`"source-layer"`, `background`'s
//!   `"background-color"`), or an unrepresentable `minzoom`/`maxzoom` --
//!   is treated as a malformed style document rather than a construct this
//!   module merely doesn't support yet, and fails the whole
//!   `parse_and_compile` call. `symbol` layers are a deliberate exception:
//!   `compile_symbol_layer` re-derives its own `minzoom`/`maxzoom` and is
//!   dispatched through the per-layer soft-skip path instead, because real
//!   `symbol` layers (e.g. Mapbox Streets-v12's `continent-label`, with a
//!   fractional `"minzoom": 0.75`) can hit this same "unrepresentable zoom
//!   bound" shape far more often than `background`/`fill`/`line` layers do
//!   in practice, and one such layer shouldn't be able to fail the whole
//!   style.

pub mod color;
pub mod config;
pub mod expression;
pub mod filter;
pub mod lut;

pub use color::parse_css_color;
pub use config::{parse_imports, Import, ResolvedConfig};
pub use expression::compile_zoom_value;
pub use filter::compile_filter;
pub use lut::ColorLut;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::style::{
    CirclePaint, DashPattern, ExtrusionLight, ExtrusionLightTable, ExtrusionValue, Filter, LightAnchor, LineCap, LineJoin, Rgba, RotationAlignment,
    Style, StyleLayer, SymbolPlacement, SymbolPlacementValue, TranslateAnchor, ZoomValue, DEFAULT_TEXT_MAX_WIDTH_EM,
    EXTRUSION_MIN_ZOOM,
};

use expression::{color_leaf, dash_leaf, emissive_strength_leaf, number_leaf, Scope};

/// Parses a style-spec JSON document (e.g. the vendored
/// `examples/liberty-style.json`) and compiles its `background`/`fill`/
/// `line` layers into a `Style`, preserving the JSON's layer order (which
/// *is* the painter's draw order).
///
/// A document with a top-level `"imports"` array contributes each imported
/// document's own `layers` FIRST, in declaration order, with the top-level
/// array drawn over them -- except where a top-level layer names a `"slot"`
/// matching one of the imported document's `type: "slot"` markers, in which
/// case it interleaves into the imported stack at that marker instead; see
/// the loop below for what that composition does and does not implement. A
/// Mapbox Standard style keeps its entire basemap there
/// (`examples/mapbox-standard-dark-style.json`: 150 imported
/// layers under an 88-layer overlay), and each import's layers are compiled
/// against that import's own resolved `config` and its document's own
/// `lights`, which is what lets their `["config", k]` and
/// `["measure-light", ...]` expressions fold. A document with no `"imports"`
/// key -- every style this crate compiled before Standard, both vendored
/// fixtures included -- takes exactly the path it always did.
///
/// One JSON layer can compile to more than one `StyleLayer`: a paint
/// property that's a feature-dependent `match` expression expands into one
/// `StyleLayer` per case (see `expression::compile_zoom_value`), all sharing
/// the JSON layer's zoom range and source layer.
///
/// Returns `Err` only for a structurally broken document or layer
/// (malformed JSON, no top-level `"layers"` array, a missing required
/// field, an unrepresentable zoom bound, ...). An individual in-scope
/// layer whose `"filter"` or a paint property fails to compile because it
/// uses some construct outside this module's Phase-1 scope (e.g. a filter
/// operator or expression shape it doesn't recognize) is instead skipped
/// with a loud stderr notice, and the rest of the style's layers still
/// compile; see the module-level doc comment for the full breakdown of
/// skip vs. hard-error cases. This is what lets styles beyond the vendored
/// Liberty style (which happens to have zero such layers), like Mapbox
/// Streets-v12, load at all despite using a handful of constructs this
/// module doesn't yet support.
pub fn parse_and_compile(bytes: &[u8]) -> Result<Style> {
    // Cleared on entry and published on exit -- see `PublishDiagnostics`
    // for why the publish is a guard rather than a line at the end.
    PENDING_DIAGNOSTICS.with(|pending| pending.borrow_mut().clear());
    let _publish = PublishDiagnostics;
    let root: Value = serde_json::from_slice(bytes).context("style document is not valid JSON")?;
    let layers_json = root
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("style document has no top-level \"layers\" array"))?;

    let mut layers = Vec::new();

    // The imported documents come FIRST, in declaration order, and the
    // top-level `layers` array draws over them. A Mapbox Standard style
    // (`examples/mapbox-standard-dark-style.json`) puts its entire basemap
    // -- land, water, roads, labels: 150 layers -- in `imports[0].data`,
    // and keeps only its 88-layer overlay up top; compiling just the
    // top-level array, which is all this function used to do, renders the
    // overlay over nothing.
    //
    // Bottom-to-top by declaration order is the DEFAULT placement. The
    // style spec also lets a top-level layer name a `slot` to interleave
    // itself INTO the imported stack, at one of the markers the imported
    // document declares (the fixture's are "bottom", "middle" and "top",
    // carried as `type: "slot"` layers, which this compiler now recognizes
    // as pure insertion points rather than an unsupported layer type --
    // they compile to zero `StyleLayer`s either way, but silently, since
    // "slot" is understood and used here, not skipped for lack of support).
    //
    // Grouped by target BEFORE either document is walked, so a marker can
    // be spliced in as soon as it's reached while composing the import
    // below, in one pass, with no index arithmetic: `layer_index` is
    // load-bearing for both draw order and (since `7084ea0`) collision
    // priority, so getting relative order right here matters twice over.
    // Lighting is scene-GLOBAL in the reference -- `Style.mergeAll` hoists
    // whichever fragment declared the lights onto the root style and
    // `painter.uploadCommonLightUniforms` uploads that one pair for every 2D
    // program (`src/style/style.ts:897-967`, `src/render/painter.ts:1764`,
    // Mapbox GL JS 3.15.0) -- so the radiance is folded ONCE here, before
    // either document's layers are walked, and every layer of every document
    // is lit with it. Root-document layers included: the Standard fixture
    // keeps its basemap (and its lights) in `imports[0]` and its buildings,
    // aeroways and tunnels up top, and lighting only the former left the
    // latter drawing near-white over a black map.
    //
    // Folded in the scope of the document that DECLARED the lights, since
    // their color/intensity/direction expressions reach for that document's
    // `config`; what travels to the other documents' layers is the evaluated
    // table, not the expressions, exactly as `mergeAll` hoists evaluated
    // lights rather than stylesheet entries. So a root-document layer is lit
    // by the import's lights without inheriting the import's config.
    let imports = parse_imports(&root)?;
    let scene_lights = merge_scene_lights(&root, &imports);
    let scene_radiance = scene_lights.as_ref().and_then(|scene| {
        let declaring = match scene.declaring_import.map(|index| &imports[index].config) {
            Some(config) => Scope::root().with_config(config),
            None => Scope::root(),
        };
        scene_radiance_table(declaring.with_lights(&scene.lights))
    });
    // The one light every extrusion is shaded with, folded in the same
    // declaring-document scope as the radiance above, and stamped onto the
    // compiled extrusions by `finish_extrusions` once every layer exists.
    let extrusion_light = compile_extrusion_light(
        &root,
        scene_lights.as_ref().map(|scene| {
            let declaring = match scene.declaring_import.map(|index| &imports[index].config) {
                Some(config) => Scope::root().with_config(config),
                None => Scope::root(),
            };
            (declaring, &scene.lights)
        }),
    );
    // The radiance and nothing else. Deliberately NOT `with_lights`: the
    // merged `lights` are raw expressions reaching for the declaring
    // document's `config`, so attaching them here would only be useful with
    // that config attached too -- and that would change how the ROOT
    // document's own `["config", ...]` expressions resolve, which is a
    // different document's namespace. The radiance escapes that because it
    // is already evaluated.
    //
    // The cost is that a `["measure-light", ...]` in a root-document layer
    // never folds; it degrades loudly (a recorded notice and a literal
    // fallback, never a wrong brightness). See `measure_light_brightness`'s
    // doc comment for the full statement of the divergence and what closing
    // it would take, and
    // `a_root_layers_measure_light_degrades_loudly_rather_than_resolving`
    // for the pin.
    let scene_scope = match &scene_radiance {
        Some(radiance) => Scope::root().with_ground_radiance(radiance),
        None => Scope::root(),
    };

    // The `color-theme` grade, unlike the lighting, is per-DOCUMENT: each
    // fragment `Style` in the reference reads its own `color-theme` and
    // grades only its own layers (see `compile_color_theme`). So the root
    // document's theme is resolved here, for the top-level `layers` loop
    // below, and each import resolves its own further down -- one PNG decode
    // per document that declares one, never one per layer.
    //
    // Folded in `scene_scope`, which for the root carries no `config`: this
    // compiler gives the root document no config namespace of its own (see
    // `parse_imports`), so a root `color-theme` reaching for `["config", k]`
    // degrades loudly rather than resolving against an import's.
    let root_lut = compile_color_theme(root.get("color-theme"), "the root document", scene_scope);
    let root_scope = match &root_lut {
        Some(lut) => scene_scope.with_lut(lut),
        None => scene_scope,
    };

    let mut slot_groups: std::collections::HashMap<&str, Vec<StyleLayer>> = std::collections::HashMap::new();
    let mut unslotted: Vec<StyleLayer> = Vec::new();
    for layer_json in layers_json {
        // An EMPTY `"slot": ""` -- what all 40 of the fixture's visible
        // top-level layers that don't target "bottom" carry -- means
        // "above the entire import", same as no `slot` field at all. A
        // name that matches none of the markers any import declares is
        // handled the same way, once the import loop below finds nothing
        // to consume it: see the flush after that loop.
        let slot = layer_json.get("slot").and_then(Value::as_str).unwrap_or("");
        let compiled = compile_single_layer(layer_json, root_scope)?;
        if compiled.is_empty() {
            continue;
        }
        match slot {
            "" => unslotted.extend(compiled),
            name => slot_groups.entry(name).or_default().extend(compiled),
        }
    }

    for import in &imports {
        // One level, and loudly not two. A fragment that imports further
        // fragments needs each nested document's own config resolved
        // against its own schema and composed in its own order, which this
        // does not do -- and quietly ignoring it would present as "the
        // basemap is missing from a document that plainly declares one",
        // which is precisely the bug this whole series exists to fix. So it
        // fails the style rather than silently rendering a partial one.
        if import.data.get("imports").is_some_and(|nested| nested.as_array().is_none_or(|a| !a.is_empty())) {
            bail!(
                "import {:?} has its own \"imports\": nested import composition is not supported \
                 (this compiler composes one level of imports only)",
                import.id
            );
        }
        // `["config", k]` and `["measure-light", ...]` inside these layers
        // resolve against THIS import's resolution and THIS imported
        // document's lights -- not the root document's, which has neither.
        // Attaching them here is the whole reason the imported layers get
        // colours rather than degrading to the literal fallbacks.
        // `scene_scope` already carries the scene's folded radiance, so this
        // adds only what is per-DOCUMENT: this import's resolved config, and
        // this document's own `lights` for `["measure-light", "brightness"]`
        // to read. The two are deliberately not merged into one: the radiance
        // is an evaluated scene-global value, while `measure-light` re-folds
        // the light's own expressions and so must see the document whose
        // `config` those expressions were written against. Identical for any
        // style with a single light-declaring fragment, which is every real
        // one.
        // This import's own `color-theme`, folded in its own config scope
        // -- the same `["config", \"theme\"]` namespace its layers read.
        // Declared before `scope` so it outlives every borrow of it.
        let config_scope = scene_scope.with_config(&import.config);
        let import_lut = compile_color_theme(import.color_theme.as_ref(), &format!("import {:?}", import.id), config_scope);
        let mut scope = match &import_lut {
            Some(lut) => config_scope.with_lut(lut),
            None => config_scope,
        };
        if let Some(lights) = import.data.get("lights") {
            scope = scope.with_lights(lights);
        }
        // A fragment with no `layers` array is one with nothing to draw
        // (a config- or source-only fragment), not a broken document.
        let imported = import.data.get("layers").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        for layer_json in imported {
            if layer_json.get("type").and_then(Value::as_str) == Some("slot") {
                // A marker contributes no `StyleLayer` of its own; it
                // exists purely to say WHERE a same-named top-level layer
                // belongs. `.remove` rather than a shared lookup: once this
                // marker has been reached, it has been reached -- a second
                // import declaring the same name (not something the
                // fixture does) would find nothing left to splice.
                if let Some(name) = layer_json.get("id").and_then(Value::as_str) {
                    if let Some(group) = slot_groups.remove(name) {
                        layers.extend(group);
                    }
                }
                continue;
            }
            layers.extend(compile_single_layer(layer_json, scope)?);
        }
    }

    // Any named slot a top-level layer asked for that no import ever
    // declared a marker for -- including every name this compiler didn't
    // just special-case, since `slot_groups` is keyed by whatever string
    // the document used -- is not silently dropped. It lands with the
    // unslotted layers, on top of everything: the style spec's own answer
    // for a slot that doesn't exist, and the only placement this compiler
    // has left to give a name it never found a marker for.
    let mut leftover: Vec<&str> = slot_groups.keys().copied().collect();
    leftover.sort_unstable();
    for name in leftover {
        if let Some(group) = slot_groups.remove(name) {
            unslotted.extend(group);
        }
    }
    layers.extend(unslotted);

    // The style's own icon sheet, carried through verbatim rather than
    // resolved here: `mapbox://sprites/...` needs an access token this
    // module has no business knowing about, and a plain HTTPS base (what
    // the vendored Liberty style declares) needs nothing at all. See
    // `Style::sprite` and `sprite::resolve_sprite_url`.
    //
    // Deliberately the ROOT document's sheet even when imports contributed
    // layers: `Style` carries one sprite, and an imported document declares
    // its own (the fixture's basemap points at `mapbox://sprites/mapbox/
    // standard/...`). Picking one sheet for icons drawn from two is a
    // sprite-resolution problem, not a layer-composition one.
    let sprite = root.get("sprite").and_then(Value::as_str).map(str::to_string);

    finish_extrusions(&mut layers, &extrusion_light);

    note_raster_layers_drawn_out_of_order(&layers);

    let sky = compile_sky(root.get("sky"), scene_scope);

    Ok(Style { layers, sprite, sky })
}

/// The root `sky` object (MapLibre style spec v8), property by property.
///
/// A style with no `sky` gets [`crate::style::Sky::default`] (the spec's
/// defaults with our pale-blue horizon). A style that declares one starts
/// from [`crate::style::Sky::spec`], so every property it leaves out keeps
/// the SPEC default.
/// A property that does not compile (a feature-dependent expression, which
/// a sky cannot be: it has no feature) keeps its default too, with a
/// `DegradedProperty` notice, so a malformed `sky` never fails the style.
fn compile_sky(sky: Option<&Value>, scope: Scope<'_>) -> crate::style::Sky {
    use crate::style::{Rgba, ZoomValue};
    let Some(object) = sky.and_then(Value::as_object) else { return crate::style::Sky::default() };
    let mut out = crate::style::Sky::spec();

    fn one<T: crate::style::ZoomExprValue>(
        name: &str,
        value: &Value,
        leaf: impl Fn(&Value, Scope<'_>) -> Result<T> + Copy,
        scope: Scope<'_>,
    ) -> Option<ZoomValue<T>> {
        match expression::compile_zoom_value(value, leaf, scope) {
            Ok(cases) if cases.len() == 1 && matches!(cases[0].0, Filter::Any) => {
                cases.into_iter().next().map(|(_, v)| v)
            }
            Ok(_) => {
                let detail = format!("sky.{name} depends on feature data, which a sky has none of; using the spec default");
                eprintln!("style_spec: {detail}");
                push_diagnostic(DiagnosticKind::DegradedProperty, None, detail);
                None
            }
            Err(error) => {
                let detail = format!("sky.{name} did not compile ({error}); using the spec default");
                eprintln!("style_spec: {detail}");
                push_diagnostic(DiagnosticKind::DegradedProperty, None, detail);
                None
            }
        }
    }
    let colour = |name: &str, slot: &mut ZoomValue<Rgba>| {
        if let Some(v) = object.get(name).and_then(|v| one(name, v, expression::color_leaf, scope)) {
            *slot = v;
        }
    };
    colour("sky-color", &mut out.sky_color);
    colour("horizon-color", &mut out.horizon_color);
    colour("fog-color", &mut out.fog_color);
    let number = |name: &str, slot: &mut ZoomValue<f32>| {
        if let Some(v) = object.get(name).and_then(|v| one(name, v, expression::number_leaf, scope)) {
            *slot = v;
        }
    };
    number("fog-ground-blend", &mut out.fog_ground_blend);
    number("horizon-fog-blend", &mut out.horizon_fog_blend);
    number("sky-horizon-blend", &mut out.sky_horizon_blend);
    number("atmosphere-blend", &mut out.atmosphere_blend);
    out
}

/// Says so, once per style, when a `raster` layer is declared ABOVE
/// geometry it will nevertheless be drawn BELOW.
///
/// # The limitation this makes visible
///
/// `render::raster_pipeline` draws every raster tile as one ordered group
/// before the vector meshes, rather than at the raster layer's own index
/// in `style.layers`. That is forced rather than chosen: since `aa20836`
/// nothing in the tile pass writes depth, so painter's order is the entire
/// occlusion model, and a raster layer -- which needs a fragment-visible
/// texture -- cannot join the single pipeline the other four layer kinds
/// share. See `render::raster_pipeline`'s module docs.
///
/// For the case this was built for the two orders coincide: a basemap is
/// the bottom of the style. Liberty puts `natural_earth` at index 1 of
/// 111, under everything, and a satellite basemap is index 0 or 1 by
/// definition. For a raster OVERLAY -- a weather layer over roads, say --
/// they do not, and the map will be wrong in a way nothing else would
/// report.
///
/// `background` is not counted as geometry above: it is not drawn as a
/// layer at all, it is the frame's clear colour (`map_view::paint`
/// resolves it separately), so a raster layer under a background is still
/// the lowest thing actually drawn.
///
/// Stderr only, and NOT `record_skipped_layer` -- the layer is being
/// drawn, just not exactly where it was asked for, and filing it as a skip
/// would make every consumer's skipped-layer count wrong.
fn note_raster_layers_drawn_out_of_order(layers: &[StyleLayer]) {
    let mut geometry_seen = false;
    for layer in layers {
        match layer {
            StyleLayer::Background { .. } => {}
            StyleLayer::Raster { id, .. } if geometry_seen => {
                eprintln!(
                    "style_spec: raster layer {id:?} is declared above vector layers but will be \
                     drawn BELOW them -- this engine draws all raster tiles as one group under \
                     the vector geometry (see render::raster_pipeline). Correct for a basemap; \
                     wrong for an overlay."
                );
            }
            StyleLayer::Raster { .. } => {}
            _ => geometry_seen = true,
        }
    }
}

/// Compiles ONE style layer outside any style document -- the entry the
/// vector-overlay path (`overlay::vector_tiles`) compiles its own layers
/// through, so an overlay's `fill`, `line` and `circle` layers mean exactly
/// what they mean in a basemap style. No `["config", ..]` bindings (an
/// overlay layer has no document to read them from). An unsupported or
/// hidden layer comes back empty, with its notice in
/// [`this_thread_compile_diagnostics`] as for a whole style.
///
/// **`circle` compiles here and NOT in a basemap style.** The vendored
/// Mapbox Streets (`turning-feature`, `turning-feature-outline`) and
/// Standard (`turning-feature`, `wind-turbine-shadow`) styles carry
/// `circle` layers, and the basemap's tile bake has no circle pass: a
/// basemap `circle` stays the loud per-layer skip it always was, so no
/// compiled basemap style (and none of the pinned counts, skip lists and
/// fingerprints over the vendored ones) moves.
pub fn compile_layer_json(layer_json: &Value) -> Result<Vec<StyleLayer>> {
    compile_single_layer_with(layer_json, Scope::root(), true)
}

/// Compiles one JSON layer to the `StyleLayer`s it expands to (zero, one, or
/// several -- see the module doc comment), in `scope`.
///
/// Called once per layer while composing an import's `layers` array (with
/// that import's resolved config and the imported document's lights
/// attached to `scope`) and once per top-level layer while grouping the
/// root document's own layers by `slot` -- see `parse_and_compile`. Both
/// callers need the same per-layer skip logic (hidden, unsupported type,
/// filter/paint compile failure), so it lives here once rather than twice.
fn compile_single_layer(layer_json: &Value, scope: Scope<'_>) -> Result<Vec<StyleLayer>> {
    compile_single_layer_with(layer_json, scope, false)
}

/// [`compile_single_layer`], with `circle` layers compiled (`circles`) or
/// skipped as an unsupported type (a basemap style).
fn compile_single_layer_with(layer_json: &Value, scope: Scope<'_>, circles: bool) -> Result<Vec<StyleLayer>> {
    let id = layer_json.get("id").and_then(Value::as_str).unwrap_or("<no id>");
    let layer_type = layer_json
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("style layer {id:?} has no \"type\""))?;
    // `layout.visibility: "none"` means the style author switched this
    // layer OFF, so it contributes nothing and is dropped whole. Decided
    // here, once, rather than in each of `compile_layer`'s per-type arms:
    // it applies identically to every layer type, in scope or not. And
    // decided BEFORE any of the parsing below, so a layer that will never
    // be drawn can't fail (or even print a notice about) a style that
    // never wanted to draw it.
    //
    // Despite the style spec listing `visibility` as a plain enum, Mapbox
    // Standard writes it as a full EXPRESSION -- e.g. `["case", ["config",
    // "showPlaceLabels"], "visible", "none"]` -- to gate a whole layer
    // category on one config key. `.as_str()` on that array is `None`,
    // which used to read as "not `\"none\"`, so visible": every config-
    // gated layer drew regardless of the config (see git history for the
    // bug this fixed -- stray "Singapore Island"/"Marina Reservoir" labels
    // with `showPlaceLabels: false`). So this folds `visibility` as an
    // expression, in `scope`, the same machinery every other `["config",
    // k]` site already uses -- rather than reading it as a bare string.
    // A literal `"visible"`/`"none"` folds to itself (`eval_inner`'s
    // "bare JSON literal is its own value" branch), so the plain-string
    // case -- the overwhelmingly common one, and the only one either
    // vendored fixture uses -- is unchanged.
    //
    // Per the reference (`StyleLayer.possiblyEvaluateVisibility`,
    // `src/style/style_layer.ts`, Mapbox GL JS 3.15.0), `visibility` is
    // evaluated exactly ONCE, at `{zoom: 0}`, when the layer is
    // constructed -- never re-evaluated per frame or per zoom. Folding it
    // here, once, at compile time in `scope` (whose `zoom` is 0 unless a
    // caller changed it, and no caller does before this point) matches
    // that: there is no such thing as a layer whose visibility differs at
    // z3 vs. z15.
    //
    // A visibility expression the folder cannot reduce to a constant is
    // NOT treated as "visible" (the old bug, just relocated) and NOT
    // silently degraded to a literal the way `or_degrade` degrades a
    // colour or width: there is no safe literal to degrade a boolean
    // on/off switch TO, and this module's own convention for "a construct
    // it doesn't understand" is the loud per-layer skip below, not a
    // guess. So an unfoldable `visibility` hides the layer, loudly --
    // safer than the alternative given what this whole fix exists to
    // stop: a config-gated category drawing on screen that the reference
    // has turned off.
    let visibility = layer_json.get("layout").and_then(|layout| layout.get("visibility"));
    let hidden = match visibility {
        None => false,
        Some(Value::String(literal)) => literal == "none",
        Some(expr) => match expression::fold_to_json_scalar(expr, scope) {
            Ok(Value::String(folded)) => folded == "none",
            Ok(other) => {
                let notice = format!(
                    "hiding layer {id:?}: layout.visibility folded to {other}, not a string"
                );
                eprintln!("style_spec: {notice}");
                record_hidden_layer(id, notice);
                true
            }
            Err(error) => {
                let notice = format!(
                    "hiding layer {id:?}: layout.visibility did not fold to a constant: {error:#}"
                );
                eprintln!("style_spec: {notice}");
                record_hidden_layer(id, notice);
                true
            }
        },
    };
    if hidden {
        return Ok(Vec::new());
    }
    let drawn = matches!(layer_type, "background" | "fill" | "line" | "symbol" | "fill-extrusion" | "raster")
        || (circles && layer_type == "circle");
    if !drawn {
        // Not an error: real styles are full of model/circle/heatmap
        // layers this renderer has no equivalent for. In the vendored
        // Liberty style this now skips 0 of 111 layers: `raster`
        // `natural_earth` used to be the one, and is now compiled
        // -- `symbol` layers (25 of them) are in scope and
        // dispatched below, and so, since the fill-extrusion fallback, is
        // `fill-extrusion` `building-3d`, which compiles to its flat
        // footprint (see `FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED`
        // for what that keeps and what it throws away). `type: "slot"` markers are a
        // deliberate exception to this notice: `parse_and_compile` handles
        // them itself, before ever reaching here, because it needs their
        // POSITION (to splice in a same-named top-level layer), not just
        // the fact that they contribute nothing. A "slot" layer reaching
        // this function at all (e.g. one this module's own tests compile
        // directly) still contributes nothing, silently -- it is understood
        // and used elsewhere in this module, not merely unsupported.
        if layer_type != "slot" {
            let notice = format!("skipping layer {id:?}: unsupported layer type {layer_type:?}");
            eprintln!("style_spec: {notice}");
            record_skipped_layer(id, notice);
        }
        return Ok(Vec::new());
    }
    // `compile_layer` itself turns a filter/paint-property compile
    // failure (some in-scope-but-unsupported construct, e.g. a filter
    // operator or expression shape it doesn't recognize -- Mapbox
    // Streets-v12's "landcover" layer, with a zoom-dependent match
    // filter, is a real example) into a loud stderr skip and an empty
    // `Ok(Vec::new())` rather than an `Err`, so it never reaches here.
    // What *does* still propagate as `Err` is a structurally broken
    // layer (no "source-layer", an unrepresentable zoom bound, ...),
    // which stays a hard failure of the whole style, same as before.
    compile_layer(id, layer_type, layer_json, scope).with_context(|| format!("failed to compile style layer {id:?}"))
}

/// Absorbs a constant-folder refusal into a literal fallback, keeping the
/// layer, and returns every other error untouched.
///
/// The two are not the same failure and must not have the same consequence.
/// An expression shape this module does not implement is an unsupported
/// CONSTRUCT: `try_or_skip!` drops the layer, loudly, and that is right --
/// the alternative is drawing a road class in a colour nobody asked for.
/// An expression the constant folder understood and could not reduce is a
/// different thing: the imported Mapbox Standard document reaches for the
/// folder in 796 places, so a single form the folder cannot yet handle
/// would take most of the style down with it if it were treated as an
/// unsupported construct. That degrades the PROPERTY to a literal instead,
/// exactly as `line-dasharray` degrades to solid rather than removing a
/// road class from the map.
fn or_degrade<T: Clone>(
    result: Result<Vec<(Filter, ZoomValue<T>)>>,
    layer_id: &str,
    property: &str,
    fallback: T,
) -> Result<Vec<(Filter, ZoomValue<T>)>> {
    match result {
        Err(error) if expression::is_unfoldable(&error) => {
            let notice = format!(
                "layer {layer_id:?}: {property} could not be folded to a value, \
                 falling back to a literal and keeping the layer: {error:#}"
            );
            eprintln!("style_spec: {notice}");
            record_degradation(notice);
            Ok(vec![(Filter::Any, ZoomValue::constant(fallback))])
        }
        other => other,
    }
}

/// One thing `parse_and_compile` had to do to a style document that the
/// document did not ask for: a layer dropped, a layer hidden, or a
/// property degraded to a literal.
///
/// This is the STRUCTURED form of the notices this module has always
/// printed to stderr. The text in `detail` is the same string that goes to
/// stderr (minus the `style_spec: ` prefix), so an existing consumer
/// grepping the terminal and a new one reading `compile_diagnostics()` see
/// the same words.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StyleDiagnostic {
    pub kind: DiagnosticKind,
    /// The style layer's `"id"`, where the notice is about one layer.
    /// `None` for a document-wide notice (an unresolvable `lights` block,
    /// a malformed LUT payload).
    pub layer: Option<String>,
    pub detail: String,
}

/// What a [`StyleDiagnostic`] cost the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticKind {
    /// The layer does not draw at all: it uses a construct this module
    /// does not implement.
    SkippedLayer,
    /// The layer does not draw because the style author (or a `config` key
    /// they gated it on) switched it off. Not a gap in this crate.
    HiddenLayer,
    /// The layer DOES draw, with one property replaced by a literal
    /// fallback because the constant folder could not reduce it.
    DegradedProperty,
}

/// Every diagnostic from the most recent `parse_and_compile` call.
///
/// Bounded and process-global. `parse_and_compile` clears it on entry, so
/// what comes back describes ONE compile -- the last one to start. Two
/// compiles racing on different threads will interleave into each other;
/// nothing in this crate does that, and a style compile is not something
/// an application does at frame rate, but a caller who compiles styles
/// concurrently should read this as advisory rather than exact.
///
/// Capped at `MAX_DIAGNOSTICS` entries. A style whose every layer fails is
/// a broken style, not a diagnostics problem, and an unbounded log here
/// would be its own slow leak in the very build this was added to measure.
static COMPILE_DIAGNOSTICS: std::sync::Mutex<Vec<StyleDiagnostic>> =
    std::sync::Mutex::new(Vec::new());

thread_local! {
    /// Where a compile in progress accumulates its diagnostics, before
    /// `PublishDiagnostics` copies the finished set into
    /// `COMPILE_DIAGNOSTICS`.
    ///
    /// Thread-local, not straight into the global, for the same reason
    /// `DEGRADATIONS` below is: `parse_and_compile` is synchronous, so a
    /// thread-local is exactly scoped to ONE compile. Two compiles on two
    /// threads therefore cannot interleave their notices into each other;
    /// the worst they can do is race over which finished set the global
    /// ends up holding, and either answer is a coherent whole compile.
    static PENDING_DIAGNOSTICS: std::cell::RefCell<Vec<StyleDiagnostic>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The ceiling on one compile's diagnostics. The vendored Mapbox Standard
/// Dark fixture, this crate's worst case, produces on the order of 100. A
/// style whose every layer fails is a broken style, not a diagnostics
/// problem, and an unbounded log here would be its own slow leak in the
/// very builds these counters were added to measure.
const MAX_DIAGNOSTICS: usize = 1024;

/// Copies the finished compile's diagnostics into `COMPILE_DIAGNOSTICS` on
/// the way out of `parse_and_compile`, however it exits.
///
/// A `Drop` guard rather than a line at the end because
/// `parse_and_compile` returns early through `?` in a dozen places, and a
/// document that failed structurally partway through still produced real
/// notices about the layers it got to -- which are exactly what a caller
/// debugging that failure wants.
struct PublishDiagnostics;

impl Drop for PublishDiagnostics {
    fn drop(&mut self) {
        // Cloned, not taken: the thread-local stays populated so
        // `last_compile_diagnostics` can assert on THIS thread's compile
        // without racing whatever else is holding the global.
        let finished = PENDING_DIAGNOSTICS.with(|pending| pending.borrow().clone());
        if let Ok(mut log) = COMPILE_DIAGNOSTICS.lock() {
            *log = finished;
        }
    }
}

/// Every diagnostic the most recent style compile produced.
///
/// Process-global and last-compile-wins. Written once, whole, at the end
/// of each `parse_and_compile`, so what comes back is always one coherent
/// compile -- but with two compiles racing on different threads, which one
/// is a matter of timing. Nothing in this crate compiles styles
/// concurrently; a caller that does should read this as advisory.
#[must_use]
pub fn compile_diagnostics() -> Vec<StyleDiagnostic> {
    COMPILE_DIAGNOSTICS.lock().map(|log| log.clone()).unwrap_or_default()
}

/// Inserts the canonical Mapbox Streets 3D-building layer (R14) directly
/// above the layer with id "building": filter extrude == "true", height
/// and min_height from the tile, minzoom 15, opacity ramping 15 -> 15.5.
/// The vendored style is never edited; callers opt in.
/// Errors if there is no "building" layer, or a "building-3d" layer already exists.
///
/// The source is the `building` layer's own `source`, so a renamed source
/// keeps working. When that source is an OpenMapTiles one (its features
/// carry `render_height`, not Mapbox's `height`/`extrude`), the layer reads
/// `render_height`/`render_min_height` and filters out `hide_3d` outlines,
/// whose `building:part` pieces are mapped separately -- the same clause
/// the vendored Liberty `building-3d` carries (Ruling, Task 7 fix round 1).
/// The colour stays Streets' own `building` `fill-color` either way.
pub fn with_3d_buildings(doc: &mut Value) -> Result<()> {
    const ID: &str = "building-3d";
    let (source, openmaptiles) = {
        let layers = doc
            .get("layers")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("style document has no top-level \"layers\" array"))?;
        if layers.iter().any(|l| l.get("id").and_then(Value::as_str) == Some(ID)) {
            bail!("style already has a {ID:?} layer");
        }
        let building = layers
            .iter()
            .find(|l| l.get("id").and_then(Value::as_str) == Some("building"))
            .ok_or_else(|| anyhow::anyhow!("style has no \"building\" layer to extrude"))?;
        let source = building
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("the \"building\" layer has no source"))?
            .to_owned();
        let openmaptiles = is_openmaptiles_source(doc, &source);
        (source, openmaptiles)
    };
    let (filter, height, base) = if openmaptiles {
        (serde_json::json!(["!=", ["get", "hide_3d"], true]), "render_height", "render_min_height")
    } else {
        (serde_json::json!(["==", ["get", "extrude"], "true"]), "height", "min_height")
    };
    let layer = serde_json::json!({
        "id": ID, "type": "fill-extrusion", "source": source, "source-layer": "building", "minzoom": 15,
        "filter": filter,
        "paint": {
            "fill-extrusion-color": "hsl(20, 15%, 85%)",
            "fill-extrusion-height": ["get", height],
            "fill-extrusion-base": ["get", base],
            "fill-extrusion-opacity": ["interpolate", ["linear"], ["zoom"], 15, 0, 15.5, 0.6]
        }
    });
    let layers = doc["layers"].as_array_mut().expect("checked above");
    let at = layers
        .iter()
        .position(|l| l.get("id").and_then(Value::as_str) == Some("building"))
        .expect("checked above");
    layers.insert(at + 1, layer);
    Ok(())
}

/// Whether the style source `name` serves the OpenMapTiles schema: named
/// `openmaptiles`, or pointing at an OpenMapTiles/OpenFreeMap endpoint.
/// Anything else (Mapbox's `composite`, `mapbox://` URLs) is read as the
/// Mapbox Streets schema.
fn is_openmaptiles_source(doc: &Value, name: &str) -> bool {
    let omt = |s: &str| {
        let s = s.to_ascii_lowercase();
        s.contains("openmaptiles") || s.contains("openfreemap")
    };
    if omt(name) {
        return true;
    }
    let Some(source) = doc.get("sources").and_then(|s| s.get(name)) else { return false };
    let url = source.get("url").and_then(Value::as_str).is_some_and(omt);
    let tiles = source
        .get("tiles")
        .and_then(Value::as_array)
        .is_some_and(|t| t.iter().filter_map(Value::as_str).any(omt));
    url || tiles
}

/// This thread's most recent compile's diagnostics, unraceable by any
/// other thread: what a caller that just ran `parse_and_compile` on this
/// thread wants (the story timeline's `validate_style_edits` reads it to
/// attribute a layer an edit broke to that edit).
///
/// Read it right after the compile, before any `.await` or other yield:
/// on a multi-thread executor a task can resume on a different thread,
/// whose list belongs to some other compile (or to none).
#[must_use]
pub fn this_thread_compile_diagnostics() -> Vec<StyleDiagnostic> {
    PENDING_DIAGNOSTICS.with(|pending| pending.borrow().clone())
}

/// This thread's most recent compile's diagnostics, unraceable by any
/// other thread. The test-side counterpart to `compile_diagnostics`.
#[cfg(test)]
fn last_compile_diagnostics() -> Vec<StyleDiagnostic> {
    PENDING_DIAGNOSTICS.with(|pending| pending.borrow().clone())
}

fn push_diagnostic(kind: DiagnosticKind, layer: Option<&str>, detail: String) {
    PENDING_DIAGNOSTICS.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.len() < MAX_DIAGNOSTICS {
            pending.push(StyleDiagnostic { kind, layer: layer.map(str::to_owned), detail });
        }
    });
}

/// Records a layer this compile dropped, and why. The `detail` is the same
/// sentence the caller printed to stderr.
fn record_skipped_layer(id: &str, detail: String) {
    push_diagnostic(DiagnosticKind::SkippedLayer, Some(id), detail);
}

/// Records a layer this compile left out because it is switched off.
fn record_hidden_layer(id: &str, detail: String) {
    push_diagnostic(DiagnosticKind::HiddenLayer, Some(id), detail);
}

#[cfg(test)]
thread_local! {
    /// Every notice `or_degrade` emitted on this thread since
    /// `take_degradations` last drained it.
    ///
    /// A degradation only ever announced itself on stderr, which no test can
    /// see -- so "the Mapbox Standard fixture compiles with ZERO folder
    /// degradations" was a claim a human had to verify by reading the terminal,
    /// and a single colour property quietly falling back to black changed no
    /// layer count and no asserted colour. That is exactly how a real defect
    /// (`recreation-polygon` painting opaque black under a `dusk` config) came
    /// within one review of shipping. `the_vendored_mapbox_standard_dark_fixture_
    /// compiles_with_no_folder_degradations` now fails on its own.
    ///
    /// Thread-local, not a global counter: the test harness runs each test on
    /// its own thread and `parse_and_compile` is synchronous, so a thread-local
    /// is exactly scoped to one compile and cannot be raced by tests running in
    /// parallel.
    static DEGRADATIONS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Records a degradation -- a property the constant folder could not
/// reduce, replaced by a literal with the layer kept.
///
/// Two destinations, deliberately. The process-global
/// `COMPILE_DIAGNOSTICS` is always on and is what `compile_diagnostics()`
/// (and, through it, the `map.get_diagnostics` agent function) reads. The
/// `DEGRADATIONS` thread-local is a test affordance and stays test-only:
/// the harness runs each test on its own thread, which is what makes
/// "exactly these degradations, from exactly this compile" an assertion a
/// parallel test run cannot race. The global cannot offer that, and the
/// tests that pin the vendored fixtures depend on it.
fn record_degradation(notice: String) {
    #[cfg(test)]
    DEGRADATIONS.with(|seen| seen.borrow_mut().push(notice.clone()));
    // `layer: None` even where the notice names one: these notices are
    // built by eleven different call sites in three modules, and each
    // formats the layer id into its own sentence rather than passing it
    // separately. Parsing the id back out of prose to fill a field in
    // would be a second, drifting source of truth for something the text
    // already says.
    push_diagnostic(DiagnosticKind::DegradedProperty, None, notice);
}

/// Drains and returns this thread's recorded degradations. Call it once
/// before the compile under test to clear anything an earlier compile in
/// the same test left behind, and again after to assert on the result.
#[cfg(test)]
fn take_degradations() -> Vec<String> {
    DEGRADATIONS.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
}

/// The scene's lights, and which document declared them.
///
/// `lights` is a rebuilt `lights` array holding the winning `ambient` and
/// `directional` entries verbatim -- the shape `expression::ground_radiance`
/// reads. `declaring_import` indexes `imports` for the document those
/// entries came from, or is `None` when it was the ROOT document (which has
/// no config of its own in this compiler). Their `color`/`intensity`/
/// `direction` are ordinary expressions reaching for that document's
/// `config`, so they can only be folded in its scope.
struct SceneLights {
    lights: Value,
    declaring_import: Option<usize>,
}

/// The scene's lights, merged the way `Style.mergeAll` merges them
/// (`src/style/style.ts:897-967`, Mapbox GL JS 3.15.0): every fragment in
/// the style is walked, and the LAST ambient-typed and last
/// directional-typed entry any of them declares becomes the scene's --
/// independently of each other, so two fragments can contribute one light
/// each. `forEachFragmentStyle` (same file, ~line 995) traverses fragments
/// before the style that imported them, so the ROOT document is visited
/// last and its own `lights` win over any import's; this compiler composes
/// exactly one level of imports, so that order is imports-in-declaration-
/// order, then the root.
///
/// `None` when no document in the style declares either light -- every
/// style before Mapbox Standard, both vendored fixtures included, which is
/// what keeps them compiling to byte-identical colours.
///
/// The point of merging at all, rather than handing each import its own
/// lights: lighting is scene-GLOBAL in the reference. `mergeAll` hoists the
/// evaluated lights onto the root style and
/// `painter.uploadCommonLightUniforms` (`src/render/painter.ts:1764-1774`)
/// then uploads that one pair for every 2D program, whichever fragment the
/// layer being drawn came from. A root-document layer is lit by an imported
/// basemap's lights, and before this it was not -- so the Standard
/// fixture's near-white root fills and lines (building, aeroway, the tunnel
/// stack) drew at full brightness over a black basemap.
///
/// Selection is by `expression::light_kind` -- `type`, as in the reference,
/// falling back to `id` for an entry that carries no `type`. That is the
/// one definition `expression::find_light` reads the merged array back
/// with, deliberately shared rather than restated here.
///
/// The one place this is coarser than the reference: `declaring_import`
/// names a single document, so a scene that split its ambient and
/// directional lights across two documents with DIFFERENT configs would
/// fold both in the last contributor's. The reference evaluates each
/// fragment's lights in its own scope. No real style splits them, and this
/// compiler's one-`Scope`-per-compile shape has nowhere to put the second
/// config.
fn merge_scene_lights(root: &Value, imports: &[Import]) -> Option<SceneLights> {
    let mut ambient = None;
    let mut directional = None;
    let mut declaring_import = None;
    let documents = imports.iter().map(|import| &import.data).enumerate().map(|(index, data)| (Some(index), data));
    for (index, document) in documents.chain(std::iter::once((None, root))) {
        let Some(lights) = document.get("lights").and_then(Value::as_array) else {
            continue;
        };
        for light in lights {
            // The SAME discriminator `expression::find_light` reads these
            // entries back with. Two definitions of "which light is this"
            // is how a style whose lights carry ids other than their types
            // came to be merged correctly and then not found at all.
            match expression::light_kind(light) {
                Some("ambient") => ambient = Some(light.clone()),
                Some("directional") => directional = Some(light.clone()),
                _ => continue,
            }
            declaring_import = index;
        }
    }
    if ambient.is_none() && directional.is_none() {
        return None;
    }
    Some(SceneLights { lights: Value::Array(ambient.into_iter().chain(directional).collect()), declaring_import })
}

/// One document's `color-theme` cube, decoded, or `None` when that document
/// declares no theme.
///
/// PER-DOCUMENT, and deliberately not merged the way `merge_scene_lights`
/// merges lights: in the reference each fragment `Style` reads its own
/// `stylesheet['color-theme']` and hands the resulting cube to its OWN
/// layers (`Style._load`, `Style._reloadColorTheme`, Mapbox GL JS 3.15.0),
/// where lighting is hoisted onto the root style and shared by every
/// fragment. So a root-document layer is graded by the ROOT's theme (in the
/// Standard fixture, none) even though it is lit by the imported basemap's
/// lights.
///
/// `theme["data"]` is an ordinary expression in this same vocabulary --
/// Standard's is a `["match", ["config", "theme"], ...]` picking one base64
/// PNG per theme -- so it is folded with `scope`, which must be the
/// declaring document's (carrying its `config`), exactly as `lights` are.
///
/// Called ONCE per document, never per layer: the cube is 128 KiB and
/// decoding it is a PNG decode, which a 150-layer basemap would otherwise
/// pay for 150 times.
///
/// Every failure short of a broken document degrades loudly to `None` (the
/// style renders ungraded rather than not at all): the reference does the
/// same, warning and leaving `lut` null. An EMPTY payload is not a failure
/// -- it is Standard's own `"default"` branch, and means "no grade".
///
/// # One divergence, measured rather than assumed
///
/// The reference also grades the LIGHTS' own colours before computing
/// ground radiance (`3d-style/render/lights.ts`:
/// `directional.properties.get('color').toNonPremultipliedRenderColor(...)`,
/// honouring a `color-use-theme` of its own). This does not, and for the
/// shipped fixture that is exactly a no-op: under every `lightPreset` this
/// crate has verified, `monochrome`'s cube maps white to white
/// (1.0 on every channel, checked against the decoded PNG), and the one
/// preset whose ambient is not white (`night`, `hsl(217, 100%, 0%)`) runs
/// at intensity 0, so its colour cannot reach the radiance at all. A theme
/// whose cube moved white, or a scene with a coloured light, would need
/// this closing -- and would show up as a whole-map tint, not a subtle one.
fn compile_color_theme(theme: Option<&Value>, document: &str, scope: Scope<'_>) -> Option<ColorLut> {
    let theme = theme?;
    let degrade = |reason: String| -> Option<ColorLut> {
        let notice = format!("{document}'s \"color-theme\" could not be applied, so its layers are drawn UNGRADED: {reason}");
        eprintln!("style_spec: {notice}");
        record_degradation(notice);
        None
    };
    let Some(data) = theme.get("data") else {
        return degrade("it has no \"data\"".to_string());
    };
    let payload = match expression::fold_to_json_scalar(data, scope) {
        Ok(Value::String(payload)) => payload,
        Ok(other) => return degrade(format!("its \"data\" folded to {other}, not a string")),
        Err(error) => return degrade(format!("its \"data\" would not fold: {error:#}")),
    };
    if payload.trim().is_empty() {
        // Standard's `"default"` theme, and the `match` fallback: a theme
        // that grades nothing. Not a degradation.
        return None;
    }
    match ColorLut::decode(&payload) {
        Ok(lut) => Some(lut),
        Err(error) => degrade(format!("{error:#}")),
    }
}

/// The scene's `u_ground_radiance` at every integer zoom this compiler
/// resamples a paint value onto, or `None` when there is no lighting to
/// apply at all.
///
/// Folded ONCE per style, in the scope of the document that declared the
/// lights (their `color`/`intensity`/`direction` are ordinary expressions
/// reaching for that document's `config`), and then carried on every
/// layer's `Scope` as an already-evaluated table -- see
/// `Scope::ground_radiance`. That is the reference's own shape: `mergeAll`
/// evaluates each fragment's lights in the fragment's scope and hoists the
/// RESULT, so a layer is lit by another document's lights without inheriting
/// that document's config.
///
/// `None` is the ordinary answer for every style written before Mapbox
/// Standard, both vendored fixtures included: no `lights` means no
/// radiance, which is Mapbox's own `LIGHTING_3D_MODE` gate (see
/// `expression::ground_radiance`) and what keeps those styles compiling to
/// byte-identical colours. It is also the answer when a style DOES have
/// lights but they will not fold -- a scene with only one of the two, say,
/// which the reference refuses in exactly the same way. That one is a real
/// degradation, so it is recorded and announced once for the style, and
/// every layer is drawn unlit rather than dropped.
fn scene_radiance_table(scope: Scope<'_>) -> Option<Vec<[f32; 3]>> {
    scope.lights()?;
    let mut table = Vec::with_capacity(expression::MAX_FOLD_ZOOM as usize + 1);
    for zoom in 0..=expression::MAX_FOLD_ZOOM {
        match expression::ground_radiance(scope, zoom) {
            Ok(radiance) => table.push(radiance),
            Err(error) => {
                let notice = format!(
                    "the scene's ground radiance could not be folded at zoom {zoom}, so every \
                     layer is drawn UNLIT: {error:#}"
                );
                eprintln!("style_spec: {notice}");
                record_degradation(notice);
                return None;
            }
        }
    }
    Some(table)
}

/// `apply_lighting_with_emission_ground` from `_prelude_lighting.glsl`
/// (Mapbox GL JS 3.15.0), which every 2D fragment shader this renderer has
/// an equivalent for -- `backgroundVert`, `fillFrag`, `fillOutlineFrag`,
/// `lineFrag`, `symbolFrag`, `circleFrag` -- calls on its output colour:
///
/// ```text
/// vec4 apply_lighting_with_emission_ground(vec4 color, float emissive_strength) {
///     return mix(color * u_ground_radiance, color, emissive_strength);
/// }
/// ```
///
/// which is, per channel, `lit_c = color_c * mix(radiance_c, 1, strength)`,
/// with the ALPHA left exactly as it was. `u_ground_radiance` is a vec3,
/// not a scalar: a coloured light tints the map rather than merely dimming
/// it, so the mix runs per channel.
///
/// # Why this happens at compile time
///
/// Colour, emissive strength and ground radiance are each functions of zoom
/// alone, so the lit colour is too -- there is nothing left to decide at
/// draw time, and nothing to change in the render path. Baking it here also
/// keeps the two halves of `ZoomValue::value_at_pair(z)` honest: they are
/// the value at `z` and at `z + 1`, two DIFFERENT zooms, and each has to be
/// lit with its own zoom's radiance and emissive strength. Resolving the
/// lighting once per frame instead would light both halves with one zoom's
/// radiance -- a wrong colour that appears only while the camera sits
/// between integer zooms. See
/// `each_half_of_a_lit_colour_pair_is_lit_at_its_own_zoom`.
///
/// Where the shader applies the lighting relative to `out_color *= opacity`
/// differs by program -- `fillFrag` and `lineFrag` light BEFORE it,
/// `symbolFrag` and `circleFrag` after, and `backgroundVert` has no opacity
/// multiply at all -- and it makes no difference: a per-channel RGB
/// multiply commutes with a scalar one, and this renderer applies opacity
/// to alpha only (`style::resolve_plan`), which the lighting never touches.
///
/// # What the rebuilt value looks like
///
/// One stop per integer zoom, exactly as `compile_folded` already resamples
/// every other zoom-dependent property, and lossless for the values that
/// reach it: every `ZoomValue` stop already sits at an integer zoom, so
/// sampling a piecewise-linear ramp at every integer and re-interpolating
/// reproduces it. (The true product of a colour ramp and a lighting ramp is
/// quadratic within a zoom interval where this is linear -- exact at every
/// integer zoom, sub-percent between two.)
///
/// The one representational conflict, and the side taken. A `ZoomValue` is
/// either wholly stepped or wholly blended, but the lighting factor is a
/// continuous function of the camera's real (fractional) zoom while an
/// authored colour may be a `step` with hard cutoffs. They cannot both be
/// honoured, so:
///
/// - a MULTI-STOP stepped colour keeps its steps. Blending it would put a
///   colour on screen that the style never declares -- halfway between two
///   `step` branches -- as the camera crosses the cutoff. The cost is that
///   its lighting is quantised to integer zoom buckets, which is the same
///   approximation the step itself already carried before any of this.
/// - a single-stop constant or an already-blended colour interpolates
///   whenever the lighting moves it, so the light itself is continuous. A
///   background is the case that makes this matter: its clear colour is
///   blended across `camera.zoom.fract()` in `MapView::paint`, and `land`'s
///   emissive strength halves between zoom 13 and 14.
///
/// When every baked stop comes out equal it collapses back to a single-stop
/// constant.
///
/// NOT clamped. `day` legitimately drives radiance past 1.0, and the
/// reference clips only where the fragment is written; here the only clamp
/// is the saturating cast at the final 8-bit write.
/// The `color-theme` grade and then the lighting -- IN THAT ORDER, in the
/// one place that composes them, so no call site can get it backwards.
///
/// Order is the whole point of this function existing. In the reference the
/// LUT is applied on the CPU when a paint colour is read
/// (`draw_background.ts`: `color.toPremultipliedRenderColor(lut)`) and the
/// lighting is applied afterwards, in the fragment shader, to that
/// already-graded colour. This compiler folds the lighting at compile time,
/// so the grade has to happen first here too. Backwards, every colour in a
/// themed style comes out subtly wrong and still plausible: the LUT is
/// non-linear, so grading a dimmed colour lands somewhere else in the cube
/// than dimming a graded one. No colour assertion written against a single
/// style notices. See
/// `the_lut_is_applied_before_the_lighting_multiply_not_after`.
///
/// `use_theme` is the property's own `*-color-use-theme` paint value (e.g.
/// `"line-color-use-theme"` beside `"line-color"`), or `None` where the
/// document declares none. `'none'` opts that one property out of the
/// grade, exactly as `draw_background.ts`'s `ignoreLut` does.
fn themed_and_lit_color(
    color: &ZoomValue<Rgba>,
    emissive_strength: &ZoomValue<f32>,
    use_theme: Option<&Value>,
    scope: Scope<'_>,
) -> ZoomValue<Rgba> {
    light_ground_color(&theme_graded_color(color, use_theme, scope), emissive_strength, scope)
}

/// `color` looked up through the document's `color-theme` cube, or an
/// unchanged clone when the document declares no theme (every style before
/// Mapbox Standard, both vendored fixtures included -- the same structural
/// gate the no-lights case has) or when this property opts out.
///
/// Every stop is graded independently. The cube is non-linear, so grading
/// the stops and interpolating between them is not identical to grading the
/// interpolated colour -- it is exact AT every stop and off by a fraction of
/// a step between two, the same approximation `light_ground_color` already
/// documents for its own product of two ramps. Resampling would not help:
/// the non-linearity is in colour space, not in zoom.
fn theme_graded_color(color: &ZoomValue<Rgba>, use_theme: Option<&Value>, scope: Scope<'_>) -> ZoomValue<Rgba> {
    let Some(lut) = scope.lut() else {
        return color.clone();
    };
    if ignores_color_theme(use_theme, scope) {
        return color.clone();
    }
    let stops: Vec<(u8, Rgba)> = color.stops.iter().map(|&(zoom, unlit)| (zoom, lut.apply(unlit))).collect();
    if color.is_interpolated() {
        ZoomValue::interpolated(stops)
    } else {
        ZoomValue::stepped(stops)
    }
}

/// Whether a `*-color-use-theme` value says this property ignores the LUT.
///
/// `draw_background.ts` reads it as
/// `layer.paint.get('background-color-use-theme').constantOr('default') ===
/// 'none'`: a value that is not a compile-time constant (a data-driven one)
/// falls back to `'default'`, i.e. the LUT applies. A missing property is
/// the spec default `'default'` too. So the only answer that opts out is a
/// value that folds to exactly `"none"`.
///
/// Nothing in either vendored fixture -- or in the Standard fixture -- sets
/// one; this exists because the style spec allows it and a style that used
/// it would otherwise be graded against its own instruction, which is a
/// wrong colour with nothing on screen to explain it.
fn ignores_color_theme(use_theme: Option<&Value>, scope: Scope<'_>) -> bool {
    let Some(value) = use_theme else {
        return false;
    };
    match value {
        Value::String(literal) => literal == "none",
        expression => {
            expression::fold_to_json_scalar(expression, scope).is_ok_and(|folded| folded == Value::String("none".into()))
        }
    }
}

fn light_ground_color(color: &ZoomValue<Rgba>, emissive_strength: &ZoomValue<f32>, scope: Scope<'_>) -> ZoomValue<Rgba> {
    let Some(radiance) = scope.ground_radiance() else {
        return color.clone();
    };
    // The table covers `0..=MAX_FOLD_ZOOM`, the ceiling this compiler folds
    // zoom-dependence to at all; a colour resampled by `compile_interpolate`
    // can carry a stop past it (its stops come from the style's own zooms),
    // and those reuse the deepest radiance the table holds -- which is exact
    // for any light whose own expressions have stopped varying by then, as
    // the real fixture's have.
    let deepest_stop = |zoom: u8| usize::from(zoom).min(radiance.len() - 1);
    let top = expression::MAX_FOLD_ZOOM
        .max(color.stops.last().map_or(0, |&(zoom, _)| zoom))
        .max(emissive_strength.stops.last().map_or(0, |&(zoom, _)| zoom));

    let stops: Vec<(u8, Rgba)> = (0..=top)
        .map(|zoom| {
            let unlit = color.value_at(zoom);
            let strength = emissive_strength.value_at(zoom);
            let factor = radiance[deepest_stop(zoom)].map(|channel| channel * (1.0 - strength) + strength);
            (zoom, Rgba { r: unlit.r * factor[0], g: unlit.g * factor[1], b: unlit.b * factor[2], a: unlit.a })
        })
        .collect();

    let first = stops[0].1;
    if stops.iter().all(|&(_, lit)| lit == first) {
        return ZoomValue::constant(first);
    }
    if color.is_interpolated() || color.is_constant() {
        ZoomValue::interpolated(stops)
    } else {
        ZoomValue::stepped(stops)
    }
}

/// Skips (rather than hard-fails) a filter/paint-property compile error:
/// an in-scope construct this module doesn't (yet) support, as opposed to a
/// structurally broken layer (handled by plain `?`, which still hard-fails
/// the whole style). See the module doc comment. Returns `Ok(Vec::new())`
/// from the enclosing function, so it is usable in any layer-compiling
/// function that returns `Result<Vec<StyleLayer>>`; `$id` is the layer id
/// the notice names. Each such function wraps it as a local `try_or_skip!`
/// that supplies its own `id`.
macro_rules! skip_layer_on_error {
    // `$what` is an `expr`, not a `literal`, because the flat-fill cases
    // name their properties from a prefix chosen at run time
    // (`"fill-color"` vs. `"fill-extrusion-color"`) rather than from a
    // literal at each call site. Every other call site still passes a
    // literal, which an `expr` fragment accepts unchanged.
    ($id:expr, $result:expr, $what:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                let id: &str = $id;
                let notice = format!("skipping layer {id:?}: failed to compile {}: {error:#}", $what);
                eprintln!("style_spec: {notice}");
                record_skipped_layer(id, notice);
                return Ok(Vec::new());
            }
        }
    };
}

/// The flat `StyleLayer::Fill` cases of a `fill` layer, or of a
/// `fill-extrusion` layer's footprint below `EXTRUSION_MIN_ZOOM`.
///
/// `max_zoom_cap` replaces the layer's `max_zoom` with
/// `min(max_zoom, cap)` when it is `Some` -- the extrusion's flat fallback
/// passes `EXTRUSION_MIN_ZOOM - 1`, its last bucket before the extrusion. `None` (every `fill` layer)
/// leaves the layer's own `max_zoom` untouched.
#[allow(clippy::too_many_arguments)]
fn compile_flat_fill_cases(
    id: &str,
    layer_type: &str,
    layer: &Value,
    scope: Scope<'_>,
    layer_filter: &Filter,
    source_layer: &str,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    max_zoom_cap: Option<u8>,
) -> Result<Vec<StyleLayer>> {
    // See `skip_layer_on_error`.
    macro_rules! try_or_skip {
        ($result:expr, $what:expr) => {
            skip_layer_on_error!(id, $result, $what)
        };
    }
    let paint = |key: &str| layer.get("paint").and_then(|paint| paint.get(key));
    let max_zoom = match max_zoom_cap {
        Some(cap) => Some(max_zoom.map_or(cap, |own| own.min(cap))),
        None => max_zoom,
    };
    let source_layer = source_layer.to_string();
    let (color_key, opacity_key, emissive_key, use_theme_key) = if layer_type == "fill-extrusion" {
        (
            "fill-extrusion-color",
            "fill-extrusion-opacity",
            "fill-extrusion-emissive-strength",
            "fill-extrusion-color-use-theme",
        )
    } else {
        ("fill-color", "fill-opacity", "fill-emissive-strength", "fill-color-use-theme")
    };
    // The property name as the notices spell it, quoted the way the
    // literals used to be (`"fill-color"`, with the quotes).
    let quoted = |key: &str| format!("{key:?}");
    let Some(color_json) = paint(color_key) else {
        // `fill-pattern`-only layers (`landcover_wetland`,
        // `road_area_pattern` in Liberty) have no `fill-color` at
        // all: their fill is a sprite image, which this renderer
        // can't draw. Skipping them is the same call as skipping a
        // `symbol` layer, not a parse failure. (Deliberately NOT
        // falling back to the spec's `#000000` default, which would
        // paint solid black blobs over real map content.)
        //
        // The same rule, for the same reason, on a
        // `fill-extrusion-pattern`-only extrusion: a black footprint
        // is worse than no footprint. No vendored style has one --
        // all 7 vendored `fill-extrusion` layers declare a colour.
        let notice = format!(
            "skipping {layer_type} layer {id:?}: no {} (pattern fills are not supported)",
            quoted(color_key)
        );
        eprintln!("style_spec: {notice}");
        record_skipped_layer(id, notice);
        return Ok(Vec::new());
    };
    let colors = try_or_skip!(
        or_degrade(compile_zoom_value(color_json, color_leaf, scope), id, &quoted(color_key), Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
        quoted(color_key)
    );
    let opacities = match paint(opacity_key) {
        Some(json) => try_or_skip!(
            or_degrade(compile_zoom_value(json, number_leaf, scope), id, &quoted(opacity_key), 1.0),
            quoted(opacity_key)
        ),
        None => vec![(Filter::Any, ZoomValue::constant(1.0))],
    };
    // Style-spec default `0`. Cross-producted with `colors`/
    // `opacities` below, the same way those two cross-product with
    // each other: a real `DataDrivenProperty`, and while no
    // vendored `fill` layer's `fill-emissive-strength` varies by
    // feature today, this is the same shape `line`'s does (see
    // `compile_emissive_strength`'s doc comment) -- no reason for
    // `fill` to represent it any less faithfully.
    let emissive_strengths = compile_emissive_strength(paint(emissive_key), id, &quoted(emissive_key), 0.0, scope);
    // `fill-outline-color` and `fill-antialias` are `fill`-ONLY
    // properties. The style spec gives `fill-extrusion` neither, so
    // an extrusion flattened into this same arm must never sprout an
    // outline from a key that cannot legally appear on it -- hence
    // the literal names here rather than the `*_key` prefixes the
    // shared properties above use.
    //
    // `fill-antialias` (spec default `true`) gates the outline
    // exactly as it does in MapLibre, whose `drawOutline` returns
    // before drawing anything when it is false. Only a literal
    // `false` switches it off: it is a `paint` property whose spec
    // type is boolean and which no vendored style expresses as an
    // expression, and reading it as anything cleverer would mean
    // inventing a folder result for a property whose only observed
    // use is a constant.
    let antialias = layer_type == "fill"
        && paint("fill-antialias").and_then(Value::as_bool).unwrap_or(true);
    let outline_colors: Vec<(Filter, Option<ZoomValue<Rgba>>)> = match paint("fill-outline-color") {
        Some(outline_json) if antialias => {
            let compiled = try_or_skip!(
                or_degrade(
                    compile_zoom_value(outline_json, color_leaf, scope),
                    id,
                    "\"fill-outline-color\"",
                    Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
                ),
                "\"fill-outline-color\""
            );
            compiled.into_iter().map(|(filter, value)| (filter, Some(value))).collect()
        }
        // No `fill-outline-color`, or `fill-antialias: false`:
        // no outline geometry at all. See `StyleLayer::Fill`'s
        // `outline_color` for why the spec's "defaults to
        // `fill-color`" is deliberately NOT reproduced as a
        // same-coloured stroke here.
        _ => vec![(Filter::Any, None)],
    };
    let mut compiled = Vec::new();
    for (color_filter, color) in &colors {
        for (opacity_filter, opacity) in &opacities {
            for (emissive_filter, emissive_strength) in &emissive_strengths {
                for (outline_filter, outline_color) in &outline_colors {
                    compiled.push(StyleLayer::Fill {
                        id: id.to_string(),
                        source_layer: source_layer.clone(),
                        filter: and_filters(vec![
                            layer_filter.clone(),
                            color_filter.clone(),
                            opacity_filter.clone(),
                            emissive_filter.clone(),
                            outline_filter.clone(),
                        ]),
                        color: themed_and_lit_color(color, emissive_strength, paint(use_theme_key), scope),
                        opacity: opacity.clone(),
                        // Lit through the SAME emissive strength as
                        // the fill it outlines -- `fill-emissive-
                        // strength` is one property for the layer,
                        // not one per colour -- but themed through
                        // its own `fill-outline-color-use-theme`,
                        // which Mapbox Standard spells separately
                        // from `fill-color-use-theme`. An outline
                        // left unlit would sit at full brightness
                        // on a dark basemap while the fill under it
                        // darkened; `colours_of` in this module's
                        // tests exists to force exactly this
                        // question and is what catches it.
                        outline_color: outline_color.as_ref().map(|outline| {
                            themed_and_lit_color(
                                outline,
                                emissive_strength,
                                paint("fill-outline-color-use-theme"),
                                scope,
                            )
                        }),
                        min_zoom,
                        max_zoom,
                    });
                }
            }
        }
    }
    Ok(compiled)
}

/// The `StyleLayer::Extrusion` cases of a `fill-extrusion` layer (spec
/// §2.2): one per colour case, from `EXTRUSION_MIN_ZOOM` up. The flat
/// footprint below the floor is `compile_flat_fill_cases`' job.
///
/// `group` and `light` are placeholders here: both are style-global facts
/// (an ordinal among the style's extrusion layers, and the style's light
/// table) that `finish_extrusions` stamps on once every layer is compiled.
/// So a caller that compiles layers WITHOUT `parse_and_compile` gets
/// `group: 0` and the default light on every extrusion, silently. No
/// production path does; `compile_layer` is reached only through
/// `parse_and_compile` outside tests.
fn compile_extrusion_cases(
    id: &str,
    layer: &Value,
    scope: Scope<'_>,
    layer_filter: &Filter,
    source_layer: &str,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
) -> Result<Vec<StyleLayer>> {
    // See `skip_layer_on_error`.
    macro_rules! try_or_skip {
        ($result:expr, $what:expr) => {
            skip_layer_on_error!(id, $result, $what)
        };
    }
    let paint = |key: &str| layer.get("paint").and_then(|paint| paint.get(key));
    let Some(color_json) = paint("fill-extrusion-color") else {
        // A `fill-extrusion-pattern`-only extrusion: its sides are a sprite
        // image this renderer cannot draw, and a black building is worse
        // than none -- the same call the flat `fill` arm makes on a
        // `fill-pattern`-only layer. No vendored style has one.
        let notice = format!(
            "skipping fill-extrusion layer {id:?}: no \"fill-extrusion-color\" (pattern fills are not supported)"
        );
        eprintln!("style_spec: {notice}");
        record_skipped_layer(id, notice);
        return Ok(Vec::new());
    };
    let colors = try_or_skip!(
        or_degrade(
            compile_zoom_value(color_json, color_leaf, scope),
            id,
            "\"fill-extrusion-color\"",
            Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        ),
        "\"fill-extrusion-color\""
    );
    // `fill-extrusion-opacity` is not data-driven in the style spec, so it
    // is one uniform per layer and never joins the case cross-product.
    let opacity = match paint("fill-extrusion-opacity") {
        Some(json) => {
            let mut cases = try_or_skip!(
                or_degrade(compile_zoom_value(json, number_leaf, scope), id, "\"fill-extrusion-opacity\"", 1.0),
                "\"fill-extrusion-opacity\""
            );
            if cases.len() > 1 {
                eprintln!(
                    "style_spec: layer {id:?}: \"fill-extrusion-opacity\" is not data-driven, but it expanded into {} \
                     filtered cases; using the unfiltered one (or else the first)",
                    cases.len()
                );
            }
            let pick = cases.iter().position(|(filter, _)| matches!(filter, Filter::Any)).unwrap_or(0);
            let opacity = cases.swap_remove(pick).1;
            // Read at the fractional frame zoom (`extrusion_plan`), so keep a
            // stop off the integer grid: Standard's `15 -> 0, 15.3 -> 0.8`.
            match expression::compile_off_grid_stops(json, number_leaf, scope) {
                Some(stops) => opacity.with_off_grid_stops(stops),
                None => opacity,
            }
        }
        None => ZoomValue::constant(1.0),
    };
    let emissive_strengths = compile_emissive_strength(
        paint("fill-extrusion-emissive-strength"),
        id,
        "\"fill-extrusion-emissive-strength\"",
        0.0,
        scope,
    );
    let height = compile_extrusion_value(paint("fill-extrusion-height"), 0.0, id, scope);
    let base = compile_extrusion_value(paint("fill-extrusion-base"), 0.0, id, scope);
    let vertical_gradient = !matches!(paint("fill-extrusion-vertical-gradient"), Some(Value::Bool(false)));
    let translate = compile_extrusion_translate(paint("fill-extrusion-translate"), id);
    // Style spec: `viewport` or the default `map`; anything else is `map`.
    let translate_anchor = match paint("fill-extrusion-translate-anchor").and_then(Value::as_str) {
        Some("viewport") => TranslateAnchor::Viewport,
        _ => TranslateAnchor::Map,
    };
    let min_zoom = Some(min_zoom.unwrap_or(0).max(EXTRUSION_MIN_ZOOM));

    let mut compiled = Vec::new();
    for (color_filter, color) in &colors {
        for (emissive_filter, emissive_strength) in &emissive_strengths {
            compiled.push(StyleLayer::Extrusion {
                id: id.to_string(),
                source_layer: source_layer.to_string(),
                filter: and_filters(vec![layer_filter.clone(), color_filter.clone(), emissive_filter.clone()]),
                group: 0,
                color: themed_and_lit_color(color, emissive_strength, paint("fill-extrusion-color-use-theme"), scope),
                opacity: opacity.clone(),
                height: height.clone(),
                base: base.clone(),
                vertical_gradient,
                translate,
                translate_anchor,
                light: ExtrusionLightTable::default(),
                min_zoom,
                max_zoom,
            });
        }
    }
    Ok(compiled)
}

/// `fill-extrusion-translate`, in pixels: a literal two-number array (bare
/// or `["literal", [x, y]]`) is kept; anything else -- a zoom or data
/// expression, a malformed value -- degrades to `[0, 0]` with a notice
/// naming the layer (spec deviation 4: literal translate only; no vendored
/// style translates an extrusion).
fn compile_extrusion_translate(json: Option<&Value>, id: &str) -> [f32; 2] {
    let Some(json) = json else { return [0.0, 0.0] };
    let pair = |v: &Value| -> Option<[f32; 2]> {
        let a = v.as_array()?;
        let [x, y] = a.as_slice() else { return None };
        let (x, y) = (x.as_f64()?, y.as_f64()?);
        (x.is_finite() && y.is_finite()).then_some([x as f32, y as f32])
    };
    let literal = pair(json).or_else(|| match json.as_array().map(Vec::as_slice) {
        Some([Value::String(op), inner]) if op == "literal" => pair(inner),
        _ => None,
    });
    literal.unwrap_or_else(|| {
        eprintln!(
            "style_spec: layer {id:?}: \"fill-extrusion-translate\" is not a literal [x, y]; drawn untranslated ([0, 0])"
        );
        [0.0, 0.0]
    })
}

/// `fill-extrusion-height`/`-base` (spec §2.2). A literal stays a
/// constant; a bare `["get", k]` takes the fast path; a per-feature
/// expression is kept whole; a feature-free expression (e.g. one reading
/// `config`) is folded once; anything else degrades to the default with a
/// notice rather than dropping the layer.
fn compile_extrusion_value(json: Option<&Value>, default: f32, id: &str, scope: Scope<'_>) -> ExtrusionValue {
    let Some(json) = json else { return ExtrusionValue::Constant(default) };
    if let Some(n) = json.as_f64() {
        return ExtrusionValue::Constant(n as f32);
    }
    if let Some([Value::String(op), Value::String(key)]) = json.as_array().map(Vec::as_slice) {
        if op == "get" {
            return ExtrusionValue::Get(key.clone());
        }
    }
    if json.is_array() && expression::is_per_feature_evaluable(json) {
        return ExtrusionValue::Feature { expr: std::sync::Arc::new(json.clone()), zoom_dependent: mentions_zoom(json) };
    }
    match expression::eval(json, scope, scope.zoom()).ok().and_then(|v| v.as_number().ok()) {
        Some(n) => ExtrusionValue::Constant(n as f32),
        None => {
            eprintln!("style_spec: layer {id:?}: extrusion height/base {json} is not evaluable; using {default}");
            ExtrusionValue::Constant(default)
        }
    }
}

/// Whether `json` reads `["zoom"]` anywhere.
fn mentions_zoom(json: &Value) -> bool {
    match json.as_array() {
        Some(items) => items.first().and_then(Value::as_str) == Some("zoom") || items.iter().any(mentions_zoom),
        None => false,
    }
}

/// The light every extrusion in the style is shaded with (R16).
///
/// A MapLibre root `light` object wins when present, read as literals only
/// (a non-literal field keeps its default, with a notice). Otherwise a
/// Mapbox v3 `directional` light from the merged scene lights is mapped
/// onto it: `direction [az, polar]` becomes `position [1.15, az, polar]`,
/// the anchor is `Map` (v3 lights are map-anchored), and colour and
/// intensity carry over. The `ambient` light is ignored. Any failure falls
/// back to the style-spec default.
fn compile_extrusion_light(root: &Value, scene: Option<(Scope<'_>, &Value)>) -> ExtrusionLightTable {
    let mut light = ExtrusionLight::default();
    if let Some(declared) = root.get("light").and_then(Value::as_object) {
        let notice = |field: &str, value: &Value| {
            eprintln!("style_spec: root \"light\".{field} {value} is not a literal this compiler reads; keeping its default");
        };
        if let Some(anchor) = declared.get("anchor") {
            match anchor.as_str() {
                Some("map") => light.anchor = LightAnchor::Map,
                Some("viewport") => light.anchor = LightAnchor::Viewport,
                _ => notice("anchor", anchor),
            }
        }
        if let Some(position) = declared.get("position") {
            match position.as_array().map(Vec::as_slice) {
                Some([r, a, p]) if r.is_number() && a.is_number() && p.is_number() => {
                    light.position = [position_f32(r), position_f32(a), position_f32(p)];
                }
                _ => notice("position", position),
            }
        }
        if let Some(color) = declared.get("color") {
            match color.as_str().map(|_| color_leaf(color, Scope::root())) {
                Some(Ok(rgba)) => light.color = rgba,
                _ => notice("color", color),
            }
        }
        if let Some(intensity) = declared.get("intensity") {
            match intensity.as_f64() {
                Some(n) => light.intensity = n as f32,
                None => notice("intensity", intensity),
            }
        }
        return ExtrusionLightTable::constant(light);
    }
    let Some((scope, lights)) = scene else { return ExtrusionLightTable::default() };
    let has_directional =
        lights.as_array().is_some_and(|all| all.iter().any(|l| expression::light_kind(l) == Some("directional")));
    if !has_directional {
        return ExtrusionLightTable::default();
    }
    // Per integer zoom, like `scene_radiance_table`: Standard's
    // `directional.intensity` is a `["zoom"]` interpolate (0 at z12 and 0.5
    // from z14 under `night`), so folding it once would shade every z15+
    // building with the z12 value.
    let scope = scope.with_lights(lights);
    let mut levels = Vec::with_capacity(expression::MAX_FOLD_ZOOM as usize + 1);
    for zoom in 0..=expression::MAX_FOLD_ZOOM {
        match expression::directional_light(scope, zoom) {
            Ok((color, intensity, [azimuthal, polar])) => levels.push(ExtrusionLight {
                anchor: LightAnchor::Map,
                position: [1.15, azimuthal as f32, polar as f32],
                color,
                intensity: intensity as f32,
            }),
            Err(error) => {
                eprintln!(
                    "style_spec: the directional light does not fold at z{zoom} ({error:#}); extrusions use the default light"
                );
                return ExtrusionLightTable::default();
            }
        }
    }
    ExtrusionLightTable::per_zoom(levels)
}

fn position_f32(value: &Value) -> f32 {
    value.as_f64().unwrap_or(0.0) as f32
}

/// Stamps the style-global facts `compile_extrusion_cases` left as
/// placeholders onto every `StyleLayer::Extrusion`: the style's one
/// `light`, and each one's `group` -- the ordinal of its source
/// `fill-extrusion` layer among the style's extrusion layers, in draw
/// order, so every colour case of one layer shares a group.
fn finish_extrusions(layers: &mut [StyleLayer], style_light: &ExtrusionLightTable) {
    let mut last_id: Option<String> = None;
    let mut next: u16 = 0;
    for layer in layers {
        let StyleLayer::Extrusion { id, group, light, .. } = layer else { continue };
        if last_id.as_deref() != Some(id.as_str()) {
            if last_id.is_some() {
                next = next.saturating_add(1);
            }
            last_id = Some(id.clone());
        }
        *group = next;
        *light = style_light.clone();
    }
}

/// Every `fill-extrusion-*` paint/layout property the vendored styles use
/// that `compile_extrusion_cases` reads NOTHING from, and the reason that
/// is a deliberate trade rather than a gap.
///
/// Height, base, colour, opacity, the vertical gradient and the light are
/// all read: a `fill-extrusion` layer compiles to a real
/// `StyleLayer::Extrusion` from `EXTRUSION_MIN_ZOOM` up (and to its flat
/// footprint below that, where its `minzoom` reaches lower). What is left
/// here is what MapLibre itself has no counterpart for (R11, R13):
///
/// - Mapbox v3's wall treatments -- flood light, ambient occlusion, cast
///   shadows, the wall line width, the cutoff fade. Each is a lighting or
///   post-process pass of Mapbox's own renderer, not a property of the
///   extruded geometry, and MapLibre draws the same buildings without them.
/// - Mapbox v3's geometry modifiers: `fill-extrusion-vertical-scale`,
///   `layout.fill-extrusion-edge-radius` and
///   `layout.fill-extrusion-rounded-corner-distance`. The walls stay
///   vertical and the corners sharp, which is MapLibre's picture.
///
/// Counted over all 7 `fill-extrusion` layers in the vendored styles
/// (Liberty's `building-3d`; Mapbox Standard Dark's `2d-building`,
/// `3d-building`, `indoor-building-walls`, `indoor-doors`, `indoor-rooms`,
/// `indoor-rooms-walls`).
///
/// `#[cfg(test)]` because a list of things deliberately NOT read has, by
/// construction, no reader in the compiler itself -- its one executable job
/// is `every_vendored_fill_extrusion_property_is_either_read_or_listed_as_ignored`,
/// which is what keeps it from decaying into a stale comment. It lives here
/// beside the compiler it describes rather than down in the test module
/// because that is where someone changing the compiler will look.
#[cfg(test)]
const FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED: &[&str] = &[
    // Geometry modifiers MapLibre has no counterpart for.
    "fill-extrusion-vertical-scale",
    "layout.fill-extrusion-edge-radius",
    "layout.fill-extrusion-rounded-corner-distance",
    // Mapbox v3 wall treatments (R11).
    "fill-extrusion-line-width",
    "fill-extrusion-flood-light-color",
    "fill-extrusion-flood-light-intensity",
    "fill-extrusion-flood-light-wall-radius",
    "fill-extrusion-flood-light-ground-radius",
    "fill-extrusion-ambient-occlusion-intensity",
    "fill-extrusion-ambient-occlusion-radius",
    "fill-extrusion-ambient-occlusion-wall-radius",
    "fill-extrusion-ambient-occlusion-ground-radius",
    "fill-extrusion-ambient-occlusion-ground-attenuation",
    "fill-extrusion-cast-shadows",
    "fill-extrusion-cutoff-fade-range",
];

/// The `raster` paint properties this compiler reads, parses and then does
/// NOT honour -- and, unlike
/// [`FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED`], this list has a
/// reader: [`note_ignored_raster_properties`] emits one line naming the
/// ones a given layer actually declares.
///
/// The difference in treatment is deliberate. A dropped Mapbox-only wall
/// treatment (flood light, ambient occlusion) leaves MapLibre's own
/// picture of the building, so announcing it would be noise. A dropped
/// `raster-opacity` would be a wrong picture -- which is exactly why `raster-opacity` is NOT on this
/// list, it is honoured. Everything that IS on this list changes what the
/// user sees, so silence would be the wrong default.
///
/// `raster-fade-duration` is the one worth reading the reasoning for. It
/// is not a raster feature, it is a TILE LIFECYCLE feature: MapLibre's
/// cross-fade binds a second texture per draw, keeps a `timeAdded`
/// timestamp and a fading-parent pointer on every tile, and needs a frame
/// clock driving redraws for as long as any tile is mid-fade
/// (`draw_raster.ts:157-229`). This engine's tile cache has no `timeAdded`
/// and no notion of a fading parent, so honouring it would mean building
/// a tile lifecycle, not a raster property. A tile here appears at full
/// opacity on the frame its texture lands.
///
/// The colour adjustments below are each a handful of lines in the
/// fragment stage (`raster.fragment.glsl:31-47`) and are omitted only
/// because nothing on the path to a satellite basemap asks for them.
/// `raster-resampling: nearest` is one `FilterMode`.
const RASTER_PROPERTIES_DELIBERATELY_IGNORED: &[&str] = &[
    // A tile lifecycle feature wearing a raster property's name.
    "raster-fade-duration",
    // Colour adjustment: cheap to add, wanted by nothing in scope.
    "raster-hue-rotate",
    "raster-brightness-min",
    "raster-brightness-max",
    "raster-saturation",
    "raster-contrast",
    // Sampling: this renderer always filters linearly.
    "raster-resampling",
];

/// Names, once per raster layer, whichever of
/// [`RASTER_PROPERTIES_DELIBERATELY_IGNORED`] that layer actually
/// declares. Silent for a layer that declares none, which is the common
/// case -- Liberty's `natural_earth` declares only `raster-opacity`, which
/// IS honoured, so a Liberty compile prints nothing here.
///
/// Deliberately stderr ONLY, and deliberately NOT `record_skipped_layer`.
/// That channel feeds `compile_diagnostics`, whose meaning is "this layer
/// is not being drawn" -- a raster layer that ignores `raster-contrast` is
/// being drawn, just not exactly as authored. Filing it as a skip would
/// make every consumer's skipped-layer count wrong, and two of this
/// module's own tests count exactly that.
fn note_ignored_raster_properties(id: &str, layer: &Value) {
    let declared: Vec<&str> = RASTER_PROPERTIES_DELIBERATELY_IGNORED
        .iter()
        .copied()
        .filter(|key| layer.get("paint").and_then(|paint| paint.get(key)).is_some())
        .collect();
    if declared.is_empty() {
        return;
    }
    eprintln!(
        "style_spec: raster layer {id:?}: parsed but not honoured: {} (see RASTER_PROPERTIES_DELIBERATELY_IGNORED)",
        declared.join(", ")
    );
}

fn compile_layer(id: &str, layer_type: &str, layer: &Value, scope: Scope<'_>) -> Result<Vec<StyleLayer>> {
    // See `skip_layer_on_error`.
    macro_rules! try_or_skip {
        ($result:expr, $what:expr) => {
            skip_layer_on_error!(id, $result, $what)
        };
    }

    if layer_type == "symbol" {
        // Handled first and separately, BEFORE the background/fill/line-
        // shared `zoom_bound` calls below: `compile_symbol_layer` re-derives
        // its own minzoom/maxzoom (see its doc comment), so a symbol layer
        // never touches that shared hard-`?` parse. Symbol layers are new to
        // this module's in-scope set (Task 4), and unlike fill/line, plenty
        // of them use constructs (line placement, `concat`-built icon/text)
        // this module doesn't support -- if such a layer hard-failed here as
        // fill/line's structurally-broken case does, ONE such layer would
        // kill loading of the entire 155-layer style. Routing through
        // `try_or_skip` instead keeps
        // that a per-layer soft skip, the same as any other in-scope-but-
        // unsupported construct (see the module doc comment).
        let symbol_layers = try_or_skip!(compile_symbol_layer(layer, scope), "symbol layer");
        return Ok(symbol_layers);
    }

    let min_zoom = zoom_bound(layer, "minzoom")?;
    let max_zoom = zoom_bound(layer, "maxzoom")?;
    let paint = |key: &str| layer.get("paint").and_then(|paint| paint.get(key));
    // `layout`'s counterpart of `paint`. Only the `"line"` arm reads it --
    // `compile_symbol_layer` has its own, on its own JSON handle, because it
    // returns before this point.
    let layout = |key: &str| layer.get("layout").and_then(|layout| layout.get(key));

    if layer_type == "background" {
        // `background-color` has no features to be data-driven by, so a
        // multi-pair (feature-filtered) expansion is meaningless here.
        let color_json = paint("background-color")
            .ok_or_else(|| anyhow::anyhow!("background layer has no \"background-color\""))?;
        let mut colors = try_or_skip!(
            or_degrade(compile_zoom_value(color_json, color_leaf, scope), id, "\"background-color\"", Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
            "\"background-color\""
        );
        if colors.len() != 1 {
            bail!("\"background-color\" must not be feature-dependent (it expanded into {} filtered cases, but a background layer has no features to filter)", colors.len());
        }
        let (_, color) = colors.remove(0);
        // Style-spec default `0` -- see `compile_emissive_strength`'s doc
        // comment and `StyleLayer::Background::emissive_strength`'s.
        let emissive_cases = compile_emissive_strength(paint("background-emissive-strength"), id, "\"background-emissive-strength\"", 0.0, scope);
        let emissive_strength = single_case_emissive_strength(emissive_cases, id, "\"background-emissive-strength\"", 0.0);
        let color = themed_and_lit_color(&color, &emissive_strength, paint("background-color-use-theme"), scope);
        return Ok(vec![StyleLayer::Background { id: id.to_string(), color, min_zoom, max_zoom }]);
    }

    // **Beside `background`'s early return, NOT in the `match layer_type`
    // below.** Between here and there sits the `"source-layer"` lookup,
    // which is a hard `?` that fails the WHOLE style rather than skipping
    // one layer. The style spec gives a raster layer no `source-layer` --
    // it names a `source` and nothing else -- so a raster layer routed
    // through that lookup would turn every style containing one into a
    // style that refuses to load. `symbol` returns early above for the
    // same class of reason.
    if layer_type == "raster" {
        // `"source"` is the raster layer's only structural requirement.
        // Unlike `source-layer` above this is NOT a hard error for the
        // whole style: a raster layer naming no source is one layer this
        // renderer cannot draw, which is exactly what the soft per-layer
        // skip exists for.
        let Some(source) = layer.get("source").and_then(Value::as_str) else {
            let notice = format!("skipping layer {id:?}: raster layer has no \"source\"");
            eprintln!("style_spec: {notice}");
            record_skipped_layer(id, notice);
            return Ok(Vec::new());
        };
        // Style-spec default 1. A raster layer has no features, so a
        // feature-dependent expansion is as meaningless here as it is for
        // `background-color` above -- and for the same reason it is
        // rejected loudly rather than silently collapsed to one case.
        let opacity = match paint("raster-opacity") {
            Some(json) => {
                let mut cases = try_or_skip!(
                    or_degrade(compile_zoom_value(json, number_leaf, scope), id, "\"raster-opacity\"", 1.0),
                    "\"raster-opacity\""
                );
                if cases.len() != 1 {
                    bail!(
                        "\"raster-opacity\" must not be feature-dependent (it expanded into {} filtered cases, but a raster layer has no features to filter)",
                        cases.len()
                    );
                }
                cases.remove(0).1
            }
            None => ZoomValue::constant(1.0),
        };
        note_ignored_raster_properties(id, layer);
        return Ok(vec![StyleLayer::Raster {
            id: id.to_string(),
            source: source.to_string(),
            opacity,
            min_zoom,
            max_zoom,
        }]);
    }

    let layer_filter = match layer.get("filter") {
        Some(filter_json) => try_or_skip!(compile_filter(filter_json, scope), "\"filter\""),
        // 3 of Liberty's 83 non-background in-scope layers (`park`,
        // `park_outline`, `building`) have no "filter" at all, meaning
        // "every feature in the source layer".
        None => Filter::Any,
    };
    let source_layer = layer
        .get("source-layer")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{layer_type} layer has no \"source-layer\""))?
        .to_string();

    match layer_type {
        // `fill` and the flat half of `fill-extrusion` share ONE body,
        // `compile_flat_fill_cases`. Sharing it rather than copying it is
        // what makes the "no existing fill moved" claim structural instead
        // of hopeful: for `layer_type == "fill"` it reads exactly the four
        // literals this arm always read, so a plain `fill` layer takes a
        // byte-identical path.
        "fill" => compile_flat_fill_cases(id, layer_type, layer, scope, &layer_filter, &source_layer, min_zoom, max_zoom, None),
        // A `fill-extrusion` layer compiles to `StyleLayer::Extrusion` from
        // `EXTRUSION_MIN_ZOOM` up, and -- only where its own `minzoom` reaches
        // lower -- to its flat footprint below that, in its own colour and
        // opacity (spec §2.2, R1). See `compile_extrusion_cases`.
        //
        // # The boundary rule at `EXTRUSION_MIN_ZOOM`
        //
        // MapLibre draws a layer for `minzoom <= z < maxzoom` (maxzoom
        // EXCLUSIVE). The extrusion's MapLibre range is therefore
        // `[max(minzoom, 12), maxzoom)`, and it is emitted only when that is
        // non-empty: `maxzoom` absent or `> 12`. A `maxzoom <= 12` layer emits
        // no extrusion and consumes no group -- at z12 it draws nothing
        // extruded, as in MapLibre.
        //
        // Below the floor the layer is a flat `Fill` (only where `minzoom <
        // 12`) with `max_zoom = min(maxzoom, 11)`. This renderer's fill gate
        // is INCLUSIVE (`zoom > max_zoom` skips) and `layer_zoom_fade_pair`
        // fades a layer out across its last bucket, so the footprint draws
        // for zoom < 12, fading out across [11, 12), and never at 12.
        //
        // **The boundary is exclusive (Ruling, Task 7):** the flat fallback
        // draws below 12, the extrusion from 12, and no frame draws both.
        // An overlap bucket would draw the fading flat fill AND the roofs
        // at z = 0 over it, and the two z-fight. The cost is a one-frame
        // pop at 12; MapLibre has no fallback at all. For `maxzoom == 12`
        // there is no extrusion, and the fill alone fades out over z11.
        "fill-extrusion" => {
            let extrudes = max_zoom.is_none_or(|max| max > EXTRUSION_MIN_ZOOM);
            let mut compiled = Vec::new();
            if extrudes {
                compiled = compile_extrusion_cases(id, layer, scope, &layer_filter, &source_layer, min_zoom, max_zoom)?;
                if compiled.is_empty() {
                    // Skipped (pattern-only, or a colour that would not
                    // compile): the notice is already recorded, and a flat
                    // fallback would only record it a second time.
                    return Ok(compiled);
                }
            }
            if min_zoom.unwrap_or(0) < EXTRUSION_MIN_ZOOM {
                compiled.extend(compile_flat_fill_cases(
                    id,
                    layer_type,
                    layer,
                    scope,
                    &layer_filter,
                    &source_layer,
                    min_zoom,
                    max_zoom,
                    Some(EXTRUSION_MIN_ZOOM - 1),
                )?);
            }
            Ok(compiled)
        }
        "line" => {
            // Both properties fall back to the style spec's own defaults
            // when absent. `line-width`'s default of 1 is load-bearing for
            // real data: Liberty's `park_outline` declares only
            // `line-color`. `line-color`'s default (#000000) never triggers
            // in Liberty (all 67 line layers declare one) but is the spec
            // behavior, so honor it rather than erroring on a legal style.
            let colors = match paint("line-color") {
                Some(json) => try_or_skip!(
                    or_degrade(compile_zoom_value(json, color_leaf, scope), id, "\"line-color\"", Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                    "\"line-color\""
                ),
                None => vec![(Filter::Any, ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }))],
            };
            let widths = match paint("line-width") {
                Some(json) => try_or_skip!(
                    or_degrade(compile_zoom_value(json, number_leaf, scope), id, "\"line-width\"", 1.0),
                    "\"line-width\""
                ),
                None => vec![(Filter::Any, ZoomValue::constant(1.0))],
            };
            let opacities = match paint("line-opacity") {
                Some(json) => try_or_skip!(
                    or_degrade(compile_zoom_value(json, number_leaf, scope), id, "\"line-opacity\"", 1.0),
                    "\"line-opacity\""
                ),
                None => vec![(Filter::Any, ZoomValue::constant(1.0))],
            };
            // `line-gap-width`: the style spec's default is 0, i.e. no
            // casing. Compiled through the very same `compile_zoom_value`
            // call `line-width` goes through, and cross-producted below the
            // same way, because it is the same KIND of property -- a
            // data-driven, zoom-interpolatable paint number. Mapbox
            // Streets' 23 `*-case` layers ramp it with the identical
            // `["interpolate", ["exponential", 1.5], ["zoom"], ...]` shape
            // they ramp the fill line's width with, and the two must
            // resolve through one path or a casing drifts off its fill
            // between stops.
            // `layout.line-cap`, read as a LITERAL keyword and nothing
            // else. See `style::LineCap` for why this one line property is
            // deliberately not an expression: a cap is geometry, and the
            // tessellation cache a zoom-varying one would silently defeat.
            //
            // Two failure shapes, one outcome. A keyword this crate does not
            // know (there is no fourth cap in the spec, so this is a
            // malformed style) and an EXPRESSION (`["step", ["zoom"],
            // "butt", 14, "round"]`, which 31 of the vendored Mapbox Streets
            // document's layers write) both degrade to the spec default with
            // a notice, rather than taking the layer down: a road drawn with
            // the wrong cap is a road, and a road skipped is a hole in the
            // map.
            let cap = match layout("line-cap") {
                None => LineCap::default(),
                Some(json) => match json.as_str().and_then(LineCap::from_style_name) {
                    Some(cap) => cap,
                    None => {
                        // stderr only, NOT `record_degradation`. That
                        // function's notices are partitioned by
                        // `split_degradations` into "a capability this
                        // compiler deliberately lacks" and "the constant
                        // folder gave up", and this is neither: the folder
                        // was never asked, and the construct is one this
                        // module refuses on purpose (see `style::LineCap`).
                        // Filing it as a folder failure would break the
                        // fixture tests that pin the folder at zero
                        // failures, and filing it as a capability gap would
                        // need a third bucket for one keyword. The
                        // `line-dasharray` degradation two blocks down sets
                        // the precedent: notice, keep the layer, move on.
                        eprintln!(
                            "style_spec: layer {id:?}: \"line-cap\" is not a literal \
                             butt/round/square keyword ({json}); using the style-spec default"
                        );
                        LineCap::default()
                    }
                },
            };
            // `layout.line-join`, read exactly as `line-cap` above is, and
            // degraded the same way for the same reasons.
            let join = match layout("line-join") {
                None => LineJoin::default(),
                Some(json) => match json.as_str().and_then(LineJoin::from_style_name) {
                    Some(join) => join,
                    None => {
                        eprintln!(
                            "style_spec: layer {id:?}: \"line-join\" is not a literal \
                             bevel/round/miter keyword ({json}); using the style-spec default"
                        );
                        LineJoin::default()
                    }
                },
            };
            let gap_widths = match paint("line-gap-width") {
                Some(json) => try_or_skip!(
                    or_degrade(compile_zoom_value(json, number_leaf, scope), id, "\"line-gap-width\"", 0.0),
                    "\"line-gap-width\""
                ),
                None => vec![(Filter::Any, ZoomValue::constant(0.0))],
            };
            // Deliberately NOT routed through `try_or_skip!`: that macro
            // drops the WHOLE layer on Err, and a dash pattern this crate
            // can't represent must degrade to a solid line, not remove a
            // road class from the map. `dash_leaf` itself never returns
            // Err, but `compile_zoom_value` still can (a malformed
            // `step`/`match` wrapper, or an `interpolate`, which
            // `DashPattern` has no interpolator for), so absorb that to
            // `none()` too.
            let dashes = match paint("line-dasharray") {
                Some(json) => compile_zoom_value(json, dash_leaf, scope)
                    .unwrap_or_else(|_| vec![(Filter::Any, ZoomValue::constant(DashPattern::none()))]),
                None => vec![(Filter::Any, ZoomValue::constant(DashPattern::none()))],
            };
            // Style-spec default `0`. Cross-producted into the color x
            // width x opacity x dash product below -- a real
            // `DataDrivenProperty`, and the real vendored `roads`/
            // `bridge-roads` genuinely vary theirs by `class` once
            // `compile_let` inlines their feature-dependent `isHighClass`
            // binding (see `compile_let`'s and `compile_emissive_strength`'s
            // doc comments): 0.3 for motorway/trunk classes, 0.2 otherwise,
            // under this fixture's own config.
            let emissive_strengths = compile_emissive_strength(paint("line-emissive-strength"), id, "\"line-emissive-strength\"", 0.0, scope);
            // Cross-product: a `match`-expanded color has to be paired with
            // every `match`-expanded width, opacity, dash and emissive
            // strength, ANDing all five cases' filters. No list is ever
            // longer than 1 in the vendored Liberty style (no paint
            // property there uses `match` -- verified by parsing the
            // file), so this loop degenerates to a single `StyleLayer` for
            // all 67 real Liberty line layers; the general case is covered
            // by unit tests below and, in the vendored Standard fixture, by
            // `roads`/`bridge-roads` themselves.
            let mut compiled = Vec::new();
            for (color_filter, color) in &colors {
                for (width_filter, width) in &widths {
                    for (opacity_filter, opacity) in &opacities {
                        for (gap_filter, gap_width) in &gap_widths {
                            for (dash_filter, dash) in &dashes {
                                for (emissive_filter, emissive_strength) in &emissive_strengths {
                                    compiled.push(StyleLayer::Line {
                                        id: id.to_string(),
                                        source_layer: source_layer.clone(),
                                        filter: and_filters(vec![
                                            layer_filter.clone(),
                                            color_filter.clone(),
                                            width_filter.clone(),
                                            opacity_filter.clone(),
                                            gap_filter.clone(),
                                            dash_filter.clone(),
                                            emissive_filter.clone(),
                                        ]),
                                        color: themed_and_lit_color(color, emissive_strength, paint("line-color-use-theme"), scope),
                                        width: width.clone(),
                                        opacity: opacity.clone(),
                                        gap_width: gap_width.clone(),
                                        cap,
                                        join,
                                        dash: dash.clone(),
                                        min_zoom,
                                        max_zoom,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            Ok(compiled)
        }
        // MapLibre's `circle` layer (`circle_style_layer_properties`):
        // every paint property data-driven and zoom-interpolatable except
        // the two pitch keywords, cross-producted into one `Circle` per
        // case exactly as `line` above is. Defaults are the spec's.
        "circle" => {
            let defaults = CirclePaint::spec_default();
            macro_rules! cases {
                ($key:literal, $leaf:expr, $fallback:expr, $default:expr) => {
                    match paint($key) {
                        Some(json) => try_or_skip!(
                            or_degrade(compile_zoom_value(json, $leaf, scope), id, concat!("\"", $key, "\""), $fallback),
                            concat!("\"", $key, "\"")
                        ),
                        None => vec![(Filter::Any, $default)],
                    }
                };
            }
            let black = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
            let radii = cases!("circle-radius", number_leaf, 5.0, defaults.radius.clone());
            let colors = cases!("circle-color", color_leaf, black, defaults.color.clone());
            let opacities = cases!("circle-opacity", number_leaf, 1.0, defaults.opacity.clone());
            let stroke_widths = cases!("circle-stroke-width", number_leaf, 0.0, defaults.stroke_width.clone());
            let stroke_colors = cases!("circle-stroke-color", color_leaf, black, defaults.stroke_color.clone());
            let stroke_opacities = cases!("circle-stroke-opacity", number_leaf, 1.0, defaults.stroke_opacity.clone());
            let blurs = cases!("circle-blur", number_leaf, 0.0, defaults.blur.clone());
            // Lit like every other colour (`themed_and_lit_color`), with
            // one emissive case per layer, as `background` takes it.
            let emissive_cases =
                compile_emissive_strength(paint("circle-emissive-strength"), id, "\"circle-emissive-strength\"", 0.0, scope);
            let emissive_strength = single_case_emissive_strength(emissive_cases, id, "\"circle-emissive-strength\"", 0.0);
            // Both keywords take `map` or `viewport`; anything else keeps
            // the default with a notice, as `line-cap` does.
            let keyword = |key: &str, default: bool| match paint(key) {
                None => default,
                Some(Value::String(k)) if k == "map" => true,
                Some(Value::String(k)) if k == "viewport" => false,
                Some(other) => {
                    eprintln!(
                        "style_spec: layer {id:?}: {key:?} is not \"map\" or \"viewport\" ({other}); using the style-spec default"
                    );
                    default
                }
            };
            let pitch = crate::render::circles::CirclePitch {
                align_map: keyword("circle-pitch-alignment", false),
                scale_map: keyword("circle-pitch-scale", true),
            };
            if paint("circle-translate").is_some() || layout("circle-sort-key").is_some() {
                eprintln!("style_spec: layer {id:?}: circle-translate / circle-sort-key are not drawn (ignored)");
            }
            let mut compiled = Vec::new();
            for (f_radius, radius) in &radii {
                for (f_color, color) in &colors {
                    for (f_opacity, opacity) in &opacities {
                        for (f_sw, stroke_width) in &stroke_widths {
                            for (f_sc, stroke_color) in &stroke_colors {
                                for (f_so, stroke_opacity) in &stroke_opacities {
                                    for (f_blur, blur) in &blurs {
                                        compiled.push(StyleLayer::Circle {
                                            id: id.to_string(),
                                            source_layer: source_layer.clone(),
                                            filter: and_filters(vec![
                                                layer_filter.clone(),
                                                f_radius.clone(),
                                                f_color.clone(),
                                                f_opacity.clone(),
                                                f_sw.clone(),
                                                f_sc.clone(),
                                                f_so.clone(),
                                                f_blur.clone(),
                                            ]),
                                            paint: CirclePaint {
                                                radius: radius.clone(),
                                                color: themed_and_lit_color(color, &emissive_strength, paint("circle-color-use-theme"), scope),
                                                opacity: opacity.clone(),
                                                stroke_width: stroke_width.clone(),
                                                stroke_color: themed_and_lit_color(
                                                    stroke_color,
                                                    &emissive_strength,
                                                    paint("circle-stroke-color-use-theme"),
                                                    scope,
                                                ),
                                                stroke_opacity: stroke_opacity.clone(),
                                                blur: blur.clone(),
                                                pitch,
                                            },
                                            min_zoom,
                                            max_zoom,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(compiled)
        }
        // "symbol" is handled earlier, before this function's shared
        // zoom_bound/filter/source-layer parsing -- see the early return
        // above.
        other => bail!("unsupported layer type {other:?}"),
    }
}

/// Compiles a `"text-field"` layout property. Scoped to the exact shapes
/// seen in the real vendored styles (see `crate::style::TextField`'s doc
/// comment): a bare literal string, `["get", key]`, and `["coalesce",
/// ["get", k1], ["get", k2], ...]`. Anything else -- e.g. `concat`-built
/// shield text -- is an `Err`, not a panic; the caller (`compile_symbol_layer`)
/// is responsible for counting/skipping on `Err`.
/// Compiles `layout.text-font` -- the faces a symbol layer's labels are set
/// in, in preference order.
///
/// # What the property actually is
///
/// An ARRAY of face names, not one name and not a family:
/// `["DIN Pro Medium", "Arial Unicode MS Regular"]`. Mapbox's font service
/// keys its glyph-PBF ranges by the full face name, and the array is a
/// per-CODEPOINT chain -- the service takes each codepoint from the first
/// entry that has it. Turning one entry into a family a font database can
/// look up, and deciding which entry is a design choice rather than a
/// coverage tail, is `font_fallback::resolve_font_stack`'s job at shape
/// time; this stage only reads the list.
///
/// # Entries that are expressions
///
/// Mapbox Standard writes its primary entry as one --
/// `["concat", ["config", "font"], " Medium"]`, 17 layers in the vendored
/// `examples/mapbox-standard-dark-style.json` -- so an entry is folded
/// through `expression::fold_to_json_scalar` in the layer's own scope when
/// it is not already a plain string. That is the same folder every other
/// config-driven property in this compiler goes through, and it is what
/// makes the Standard styles resolve to `Inter Medium` rather than to their
/// coverage tail.
///
/// # Degradation
///
/// Never an error, and never a skipped layer. An entry that will not fold is
/// DROPPED from the stack with a recorded notice, and a `text-font` that is
/// not an array at all (a `["step", ["zoom"], [...], ...]`, which this
/// compiler has no per-zoom face to hold) degrades to the style spec's own
/// default stack. A label in the wrong face is a far smaller defect than a
/// layer with no labels -- exactly the trade `text-anchor` and `text-offset`
/// already make above.
fn compile_text_font(json: Option<&Value>, id: &str, scope: expression::Scope<'_>) -> crate::font_fallback::FontStack {
    let degrade = |what: &str| -> crate::font_fallback::FontStack {
        let notice = format!(
            "layer {id:?}: text-font {what} is not a shape this crate can read; \
             falling back to the style-spec default stack and keeping the layer"
        );
        eprintln!("style_spec: {notice}");
        record_degradation(notice);
        crate::font_fallback::default_font_stack()
    };
    let Some(json) = json else { return crate::font_fallback::default_font_stack() };
    // `["literal", ["Noto Sans Regular"]]` is the same list, quoted -- the
    // spec's own way of writing an array operand where an expression is
    // allowed.
    let entries = match json {
        Value::Array(arr) if arr.first().and_then(Value::as_str) == Some("literal") => {
            match arr.get(1).and_then(Value::as_array) {
                Some(inner) => inner.clone(),
                None => return degrade(&json.to_string()),
            }
        }
        Value::Array(arr) => arr.clone(),
        _ => return degrade(&json.to_string()),
    };
    let stack: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            Value::String(name) => Some(name.clone()),
            other => match expression::fold_to_json_scalar(other, scope) {
                Ok(Value::String(name)) => Some(name),
                _ => {
                    let notice = format!(
                        "layer {id:?}: text-font entry {other} does not fold to a face name; \
                         dropping that entry and keeping the rest of the stack"
                    );
                    eprintln!("style_spec: {notice}");
                    record_degradation(notice);
                    None
                }
            },
        })
        .collect();
    // An `["absent"]`-shaped stack, or one whose every entry was an
    // unfoldable expression, is the same situation as no property at all.
    if stack.is_empty() {
        return crate::font_fallback::default_font_stack();
    }
    stack.into()
}

pub(crate) fn compile_text_field(json: &Value) -> Result<crate::style::TextField> {
    use crate::style::TextField;
    if let Some(s) = json.as_str() {
        return Ok(TextField::Literal(s.to_string()));
    }
    let arr = json
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("\"text-field\" must be a string or expression array: {json}"))?;
    match arr.first().and_then(|v| v.as_str()) {
        Some("get") => {
            let key = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("\"get\" in text-field needs a string key: {json}"))?;
            Ok(TextField::Get(key.to_string()))
        }
        Some("coalesce") => {
            let keys = arr[1..]
                .iter()
                .map(|entry| {
                    let inner = entry
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("\"coalesce\" entries in text-field must be [\"get\", key]: {json}"))?;
                    if inner.first().and_then(|v| v.as_str()) != Some("get") {
                        bail!("\"coalesce\" entries in text-field must be [\"get\", key]: {json}");
                    }
                    inner
                        .get(1)
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| anyhow::anyhow!("\"get\" in text-field coalesce needs a string key: {json}"))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(TextField::Coalesce(keys))
        }
        // The flat `ConcatPart` form first, so every concat that compiled
        // before compiles to the identical value; only a concat with a
        // nested expression part (`airport-label`'s `coalesce`) falls back
        // to the recursive `ConcatFields`.
        Some("concat") => match compile_concat_parts(&arr[1..], "text-field", json) {
            Ok(parts) => Ok(TextField::Concat(parts)),
            Err(_) => Ok(TextField::ConcatFields(arr[1..].iter().map(compile_text_field).collect::<Result<_>>()?)),
        },
        Some("to-string") => Ok(TextField::ToStringGet(to_string_get_key(arr, json)?)),
        // `["step", ["zoom"] | ["get", key], base, t1, f1, ...]`, each output
        // itself a text field (MapLibre `Step`: the last stop <= input wins,
        // the base below the first).
        Some("step") => {
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("\"step\" in text-field must be [\"step\", input, base, t1, f1, ...]: {json}");
            }
            let input = arr[1].as_array().map(Vec::as_slice).unwrap_or_default();
            let base = Box::new(compile_text_field(&arr[2])?);
            let pairs = arr[3..].chunks(2).map(|pair| {
                let threshold = pair[0]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("\"step\" thresholds in text-field must be numbers: {json}"))?;
                Ok((threshold, compile_text_field(&pair[1])?))
            });
            match input {
                [op] if op.as_str() == Some("zoom") => Ok(TextField::ZoomStep {
                    base,
                    stops: pairs
                        .map(|pair| pair.map(|(z, field)| (z.clamp(0.0, 255.0) as u8, field)))
                        .collect::<Result<_>>()?,
                }),
                [op, key] if op.as_str() == Some("get") && key.is_string() => Ok(TextField::DataStep {
                    key: key.as_str().unwrap_or_default().to_string(),
                    base,
                    stops: pairs.collect::<Result<_>>()?,
                }),
                _ => bail!("\"step\" in text-field must step on [\"zoom\"] or [\"get\", key]: {json}"),
            }
        }
        // `["match", ["get", key], labels, field, ..., default]` on a string
        // tag.
        Some("match") => {
            let key = arr
                .get(1)
                .and_then(expression::extract_get_key)
                .ok_or_else(|| anyhow::anyhow!("\"match\" in text-field must match on [\"get\", key]: {json}"))?;
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("\"match\" in text-field must be [\"match\", input, labels, field, ..., default]: {json}");
            }
            let arms = arr[2..arr.len() - 1]
                .chunks(2)
                .map(|pair| Ok((expression::extract_match_labels(&pair[0], json)?, compile_text_field(&pair[1])?)))
                .collect::<Result<_>>()?;
            Ok(TextField::Match {
                key,
                arms,
                default: Box::new(compile_text_field(arr.last().expect("length checked"))?),
            })
        }
        // `["case", ["has", key], then, otherwise]` -- the shape every
        // OpenMapTiles label layer uses to prefer a bilingual name where the
        // tile has one. Only the single-arm form is accepted, which is the
        // only form the real styles use; a longer if/elif chain is still an
        // `Err`, and the layer is still skipped, exactly as before.
        Some("case") if arr.len() == 4 => {
            let condition = arr[1]
                .as_array()
                .filter(|c| c.first().and_then(|v| v.as_str()) == Some("has"))
                .ok_or_else(|| anyhow::anyhow!("only [\"has\", key] conditions are supported in a text-field case: {json}"))?;
            let key = condition
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("\"has\" in text-field case needs a string key: {json}"))?;
            Ok(TextField::CaseHas {
                key: key.to_string(),
                then: Box::new(compile_text_field(&arr[2])?),
                otherwise: Box::new(compile_text_field(&arr[3])?),
            })
        }
        _ => bail!("unsupported \"text-field\" expression shape: {json}"),
    }
}

/// Parses the entries of a `["coalesce", ["image", ["get", k1]], ["image",
/// ["get", k2]], ...]` icon-image expression into the fallback-ordered list
/// of keys. Shared by the bare `"coalesce"` shape and the redundant
/// `"case"`-wrapped shape below, which both bottom out in the same
/// `IconImage::CoalesceImage`.
fn parse_coalesce_image_entries(entries: &[Value], json: &Value) -> Result<Vec<String>> {
    entries
        .iter()
        .map(|entry| {
            let inner = entry.as_array().ok_or_else(|| {
                anyhow::anyhow!("\"coalesce\" entries in icon-image must be [\"image\", [\"get\", key]]: {json}")
            })?;
            if inner.first().and_then(|v| v.as_str()) != Some("image") {
                bail!("\"coalesce\" entries in icon-image must be [\"image\", [\"get\", key]]: {json}");
            }
            let get_expr = inner
                .get(1)
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("\"image\" in icon-image coalesce must wrap [\"get\", key]: {json}"))?;
            if get_expr.first().and_then(|v| v.as_str()) != Some("get") {
                bail!("\"image\" in icon-image coalesce must wrap [\"get\", key]: {json}");
            }
            get_expr
                .get(1)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("\"get\" in icon-image coalesce needs a string key: {json}"))
        })
        .collect::<Result<Vec<_>>>()
}

/// Compiles an `"icon-image"` layout property. Scoped to the exact shapes
/// seen in the real vendored styles (see `crate::style::IconImage`'s doc
/// comment): a bare literal string, `["get", key]`, the real
/// `["coalesce", ["image", ["get", k1]], ["image", ["get", k2]], ...]`
/// `maki_beta`/`maki` fallback shape (also recognized wrapped in the
/// redundant `["case", ["has", k1], <that coalesce>, ["image", ["get",
/// k2]]]` guard the real `poi-label` style uses -- semantically identical to
/// the bare coalesce, since the `case` condition and the coalesce's first
/// key always agree), a zoom-only `["step", ["zoom"], base, t1, v1,
/// ...]` over string outputs, a `["concat", ...]` sprite name assembled
/// from feature properties -- each part a literal, `["get", k]`, or
/// `["to-string", ["get", k]]` (see `crate::style::ConcatPart`), the real
/// road-shield shape; a whole-expression `["to-string", ["get", k]]` sprite
/// name (Liberty's `poi_transit`, where the `to-string` IS the expression
/// rather than one argument of a `concat`); a `["match", ["get", k],
/// <labels>, <output>, ..., <fallback>]` over string labels, each output
/// recursively compiled (Liberty's three `poi_r*` layers); and, more broadly than the
/// `maki_beta`/`maki` guard above, any `["case", ["has", k], <then>,
/// <else>]` whose `then`-branch is a
/// `coalesce` starting with `["image", X]`: `X` and the `else`-branch are
/// each recursively compiled through `compile_icon_image` (so either can be
/// a literal, `get`, `concat`, or anything else this function accepts, not
/// only the concrete shapes below), producing `CaseHas { key: k, then: X,
/// otherwise: <else> }`. The real `road-number-shield` two-generation shield
/// shape is the motivating case -- a `coalesce` of two `concat` expressions
/// as `then`, a bare `concat` (not wrapped in `["image", ...]`) as `else` --
/// but the recursion accepts more than that one shape (see `CaseHas`'s doc
/// comment for what is dropped and why).
///
/// Two more shapes, both new with Mapbox Standard: the style spec's own
/// `["image", name, options?]` expression, whose NAME is compiled
/// recursively and whose options are dropped on the record (see
/// `note_dropped_image_options`); and a `["let", name, value, ..., body]`
/// wrapper, inlined at its `["var", name]` use sites and then compiled as
/// the body it wraps.
///
/// Anything else is an `Err`, not a panic.
pub(crate) fn compile_icon_image(json: &Value) -> Result<crate::style::IconImage> {
    use crate::style::IconImage;
    if let Some(s) = json.as_str() {
        return Ok(IconImage::Literal(s.to_string()));
    }
    let arr = json
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("\"icon-image\" must be a string or expression array: {json}"))?;
    match arr.first().and_then(|v| v.as_str()) {
        Some("get") => {
            let key = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("\"get\" in icon-image needs a string key: {json}"))?;
            Ok(IconImage::Get(key.to_string()))
        }
        Some("coalesce") => {
            let keys = parse_coalesce_image_entries(&arr[1..], json)?;
            Ok(IconImage::CoalesceImage(keys))
        }
        // The real `poi-label` `icon-image` wraps the coalesce above in a
        // `case(has(K), coalesce(image(get(K)), image(get(F))), image(get(F)))`
        // guard. That guard is redundant: when K is present both branches
        // resolve to K, and when K is absent both resolve to F. Recognize
        // ONLY this exact shape (condition key must match the coalesce's
        // first key; else-branch key must match the coalesce's fallback
        // key) and unwrap it to the same `CoalesceImage` the bare shape
        // above produces. A general `case` expression is out of scope.
        Some("case") if arr.len() == 4 => {
            let condition = arr[1]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("unsupported \"icon-image\" expression shape: {json}"))?;
            let has_key = if condition.first().and_then(|v| v.as_str()) == Some("has") {
                condition.get(1).and_then(|v| v.as_str())
            } else {
                None
            };
            let then_coalesce = arr[2]
                .as_array()
                .filter(|inner| inner.first().and_then(|v| v.as_str()) == Some("coalesce"));
            let else_key = arr[3]
                .as_array()
                .filter(|inner| inner.first().and_then(|v| v.as_str()) == Some("image"))
                .and_then(|inner| inner.get(1))
                .and_then(|v| v.as_array())
                .filter(|get_expr| get_expr.first().and_then(|v| v.as_str()) == Some("get"))
                .and_then(|get_expr| get_expr.get(1))
                .and_then(|v| v.as_str());

            match (has_key, then_coalesce, else_key) {
                (Some(has_key), Some(then_coalesce), Some(else_key)) => {
                    let keys = parse_coalesce_image_entries(&then_coalesce[1..], json)?;
                    if keys.len() == 2 && keys[0] == has_key && keys[1] == else_key {
                        Ok(IconImage::CoalesceImage(keys))
                    } else {
                        bail!("unsupported \"icon-image\" expression shape: {json}");
                    }
                }
                _ => {
                    // The shield shape: the else-branch is a bare `concat`
                    // rather than an `["image", ["get", k]]`, and the
                    // coalesce entries wrap `concat` expressions. Compile it
                    // as a CaseHas over two Concats, keeping only the FIRST
                    // coalesce entry (see CaseHas's doc comment).
                    let (Some(has_key), Some(then_coalesce)) = (has_key, then_coalesce) else {
                        bail!("unsupported \"icon-image\" expression shape: {json}");
                    };
                    let first_image = then_coalesce
                        .get(1)
                        .and_then(|v| v.as_array())
                        .filter(|img| img.first().and_then(|v| v.as_str()) == Some("image"))
                        .and_then(|img| img.get(1))
                        .ok_or_else(|| anyhow::anyhow!("unsupported \"icon-image\" expression shape: {json}"))?;
                    eprintln!(
                        "style_spec: icon-image {json}: compiling the shield case on \
                         [\"has\", {has_key:?}] alone; its image-availability test is not \
                         evaluable where the name is resolved and is dropped"
                    );
                    Ok(IconImage::CaseHas {
                        key: has_key.to_string(),
                        then: Box::new(compile_icon_image(first_image)?),
                        otherwise: Box::new(compile_icon_image(&arr[3])?),
                    })
                }
            }
        }
        Some("step") => {
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("\"step\" in icon-image must have the form [\"step\", [\"zoom\"], base, stop1, out1, ...]: {json}");
            }
            // Only a ZOOM step is in scope at the top level. Every
            // `icon-image` step in both vendored styles uses `["zoom"]`, and
            // without this guard a top-level `["step", ["get", k], ...]`
            // would be silently misread as one -- comparing a feature
            // property against what the code then treats as zoom brackets.
            // A data step belongs INSIDE a zoom bracket (see
            // `parse_data_icon_shape`), not in place of one.
            if arr[1].as_array().and_then(|input| input.first()).and_then(|v| v.as_str()) != Some("zoom") {
                bail!("\"step\" in icon-image must step on [\"zoom\"] at the top level: {json}");
            }
            // Each per-zoom output is either a plain name or a data-driven
            // shape. Parse them all as shapes, then collapse back to the
            // simpler `ZoomStep` when every one turned out to be a plain
            // name -- that keeps the existing oneway-arrow/crosswalk icons
            // on exactly the representation they already had.
            let mut stops: Vec<(u8, crate::style::DataIconShape)> = vec![(0u8, parse_data_icon_shape(&arr[2], json)?)];
            let mut i = 3;
            while i + 1 < arr.len() {
                let zoom = arr[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("\"step\" in icon-image expects numeric zoom stops: {json}"))?
                    as u8;
                stops.push((zoom, parse_data_icon_shape(&arr[i + 1], json)?));
                i += 2;
            }
            let all_plain = stops
                .iter()
                .all(|(_, shape)| matches!(shape, crate::style::DataIconShape::Name(_)));
            if all_plain {
                let names = stops
                    .into_iter()
                    .map(|(zoom, shape)| match shape {
                        crate::style::DataIconShape::Name(name) => (zoom, name),
                        _ => unreachable!("just checked every shape is a plain name"),
                    })
                    .collect();
                return Ok(IconImage::ZoomStep(ZoomValue::stepped(names)));
            }
            Ok(IconImage::ZoomOfDataIcon(stops))
        }
        Some("concat") => Ok(IconImage::Concat(compile_concat_parts(&arr[1..], "icon-image", json)?)),
        // `["to-string", ["get", k]]` as the whole sprite name -- Liberty's
        // `poi_transit`. The same expression `compile_concat_parts` already
        // accepts as one PART of a concat, here standing alone, so it shares
        // `to_string_get_key` with the concat and text-field parsers.
        Some("to-string") => Ok(IconImage::ToStringGet(to_string_get_key(arr, json)?)),
        // `["match", ["get", k], <labels>, <output>, ..., <fallback>]` --
        // Liberty's three `poi_r*` layers. Labels may be written as a single
        // value or as an array of them (the style spec allows both); both
        // compile to the same label LIST, so `resolve` has one rule. Each
        // output goes back through `compile_icon_image`, so an arm can name
        // its sprite with a literal or a `get` -- the two forms the real
        // styles use -- without a second output type.
        Some("match") => {
            // [_, input, (label, output)+, fallback]: at least one pair, and
            // the pairs must come out even once the fallback is set aside.
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!(
                    "\"match\" in icon-image must have the form [\"match\", [\"get\", key], \
                     label1, out1, ..., fallback]: {json}"
                );
            }
            let key = arr[1]
                .as_array()
                .filter(|input| input.first().and_then(|v| v.as_str()) == Some("get"))
                .and_then(|input| input.get(1))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("\"match\" in icon-image must match on [\"get\", key]: {json}"))?;
            let mut arms = Vec::new();
            let mut i = 2;
            while i + 1 < arr.len() - 1 {
                arms.push((parse_match_labels(&arr[i], json)?, compile_icon_image(&arr[i + 1])?));
                i += 2;
            }
            Ok(IconImage::MatchGet {
                key: key.to_string(),
                arms,
                fallback: Box::new(compile_icon_image(arr.last().expect("length checked above"))?),
            })
        }
        // `["image", name]` / `["image", name, {"params": {...}}]` -- the
        // style spec's own image expression, and the shape all eleven of
        // Mapbox Standard's one-way-arrow and crosswalk layers name their
        // sprite with. Only the NAME is used here; see
        // `note_dropped_image_options` for what the options mean and why
        // this crate cannot honour them.
        //
        // The name is compiled recursively rather than required to be a
        // literal: Mapbox GL JS parses it as an arbitrary string-typed
        // expression (`style-spec/expression/definitions/image.ts`,
        // `tryParseImage`: `context.parse(args[nextArgId], ..., StringType)`),
        // and `["image", ["get", k]]` is already the shape
        // `parse_coalesce_image_entries` reads inside a `coalesce`.
        //
        // A SECOND image argument (the spec allows `["image", primary,
        // opts?, secondary, opts?]` -- `ResolvedImage`'s primary/secondary
        // variants) is not represented: `IconImage::resolve` yields one
        // name, and the secondary is a fallback used only when the primary
        // sprite is missing from the atlas, which is not knowable where a
        // name is resolved. No layer in either vendored document uses one,
        // and `note_dropped_image_options` records it if one ever appears.
        Some("image") => {
            let name = arr
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("\"image\" in icon-image needs a name argument: {json}"))?;
            note_dropped_image_options(&arr[1..], json);
            compile_icon_image(name)
        }
        // `["let", name, value, ..., body]` wrapping the whole `icon-image`
        // -- every one of the ten `["image", ..., {"params": ...}]` arrow and
        // crosswalk layers is written this way, binding a lightness channel
        // off `config` that is then referenced ONLY from inside the `params`
        // this crate drops.
        //
        // Inlined textually at each `["var", name]` use site, reusing
        // `expression::substitute_vars` -- the same primitive `compile_let`
        // uses for a feature-dependent binding -- rather than pushing a
        // lexical `Scope`: `compile_icon_image` has no `Scope` at all (it
        // resolves a sprite NAME from tags and zoom, never a folded value),
        // so substitution is the only mechanism available, and it is exact.
        // A `var` that survives into a name position is then an ordinary
        // unsupported shape and bails, as it should.
        Some("let") => {
            let bindings: Vec<(String, Value)> = expression::parse_let_bindings(arr, json)?
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect();
            let body = arr.last().expect("parse_let_bindings checked the arity");
            compile_icon_image(&expression::substitute_vars(body, &bindings))
        }
        _ => bail!("unsupported \"icon-image\" expression shape: {json}"),
    }
}

/// Announces -- and records as a degradation -- the `["image", ...]` options
/// this crate cannot honour, so the gap is visible rather than silent.
///
/// `["image", name, {"params": {"color": ...}}]` is Mapbox's RUNTIME-
/// RECOLOURABLE image. The sprite is authored as a uSVG whose `metadata`
/// declares named colour `variables`, and at rasterization time each
/// `params` entry replaces the placeholder colour that variable stands for
/// (Mapbox GL JS 3.15.0, `src/data/usvg/usvg_pb_renderer.ts`:
/// `ColorReplacements.calculate` maps `params[key]` onto
/// `variables[key].rgb_color`, and `getStyleColor` swaps it in per path).
/// The `params` are parsed as ordinary colour-typed EXPRESSIONS
/// (`style-spec/expression/definitions/image.ts`, `ImageExpression.parse`)
/// and travel with the resolved image as `RasterizationOptions`
/// (`style-spec/expression/types/image_variant.ts`).
///
/// That whole mechanism is a vector-icon one, and this crate's sprite
/// pipeline is raster: it fetches a pre-rendered PNG sheet, and a non-SDF
/// sprite is drawn in its authored colours -- `icon-color` is SDF-only, a
/// property pinned by `a_non_sdf_sprite_ignores_icon_color_entirely` in
/// `src/map_view.rs`. There is no place to apply a recolour even in
/// principle. Drawing the arrow in the colour the sheet was authored with is
/// enormously better than drawing no arrow, so the name is kept and the
/// options are dropped -- but dropped ON THE RECORD, through the same
/// `record_degradation` channel a paint property that falls back to a
/// literal uses.
fn note_dropped_image_options(args: &[Value], json: &Value) {
    let Some(options) = args.iter().find_map(Value::as_object) else {
        return;
    };
    if options.is_empty() {
        return;
    }
    let dropped: Vec<&str> = options.keys().map(String::as_str).collect();
    let notice = format!(
        "icon-image {json}: dropping the [\"image\", ...] option(s) {dropped:?} and drawing the \
         sprite in the colours the sheet was authored with -- runtime image \"params\" recolour a \
         uSVG icon's named colour variables at rasterization time, which a raster sprite sheet has \
         no equivalent of (icon-color is SDF-only here)"
    );
    eprintln!("style_spec: {notice}");
    record_degradation(notice);
}

/// Compiles the arguments of a `["concat", ...]` into `ConcatPart`s: a bare
/// string literal, `["get", key]`, or `["to-string", ["get", key]]`.
///
/// Shared by `icon-image` (road-shield sprite names) and `text-field`
/// (bilingual place and street names). They compile the identical shape, and
/// having compiled it two different ways would be how the two drift -- most
/// likely on `to-string`'s numeric rule, which is subtle and already settled
/// once. `property` only names the failing property in the error.
fn compile_concat_parts(
    args: &[Value],
    property: &str,
    json: &Value,
) -> Result<Vec<crate::style::ConcatPart>> {
    let mut parts = Vec::new();
    for entry in args {
        if let Some(text) = entry.as_str() {
            parts.push(crate::style::ConcatPart::Literal(text.to_string()));
            continue;
        }
        let inner = entry
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("unsupported {property:?} expression shape: {json}"))?;
        match inner.first().and_then(|v| v.as_str()) {
            Some("get") => {
                let key = inner
                    .get(1)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("\"get\" in concat needs a string key: {json}"))?;
                parts.push(crate::style::ConcatPart::Get(key.to_string()));
            }
            Some("to-string") => {
                let key = to_string_get_key(inner, json)?;
                parts.push(crate::style::ConcatPart::ToStringGet(key));
            }
            _ => bail!("unsupported {property:?} expression shape: {json}"),
        }
    }
    Ok(parts)
}

/// The `key` out of a `["to-string", ["get", key]]`, which is the only
/// `to-string` shape either property uses.
fn to_string_get_key(arr: &[Value], json: &Value) -> Result<String> {
    let get_expr = arr
        .get(1)
        .and_then(|v| v.as_array())
        .filter(|g| g.first().and_then(|v| v.as_str()) == Some("get"))
        .ok_or_else(|| anyhow::anyhow!("\"to-string\" must wrap [\"get\", key]: {json}"))?;
    get_expr
        .get(1)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("\"get\" in to-string needs a string key: {json}"))
}

/// The label side of one `["match", ...]` arm in an `icon-image`: either a
/// single value or an array of them, both of which the style spec allows and
/// both of which mean "the input equals any of these". Compiles to the same
/// list either way, so `IconImage::MatchGet::resolve` has one rule.
///
/// Only STRING labels are in scope. A sprite name is matched against a
/// string-valued property in every real style, and a numeric label would need
/// its own comparison against `TagValue::Number` -- refusing it is an `Err`
/// the caller degrades on, never a silent mismatch that hands every feature
/// the fallback icon.
fn parse_match_labels(value: &Value, json: &Value) -> Result<Vec<String>> {
    if let Some(label) = value.as_str() {
        return Ok(vec![label.to_string()]);
    }
    let labels = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("a \"match\" label in icon-image must be a string or an array of them: {json}"))?;
    if labels.is_empty() {
        bail!("a \"match\" label array in icon-image must not be empty: {json}");
    }
    labels
        .iter()
        .map(|label| {
            label
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow::anyhow!("only string \"match\" labels are supported in icon-image: {json}"))
        })
        .collect()
}

/// One zoom bracket of a `["step", ["zoom"], ...]` `icon-image`: a plain
/// name, a `["step", ["get", key], ...]` over string outputs, a
/// `["case", ["==", ["get", key], n], name, <else>]`, a `["match", ["get",
/// key], ...]` over string labels, or an `["image", name, options?]`
/// naming any of those. Scoped to the real city-label
/// (`settlement-major-label`/`settlement-minor-label`), one-way-arrow and
/// crosswalk shapes; `concat`/`to-string`/`coalesce`-of-`image` icon names
/// stay out of scope and are refused here.
///
/// The structural twin of `parse_data_step`, which parses the same `step`
/// shape over NUMERIC outputs -- see `crate::style::DataIconShape`'s doc
/// comment for why the two aren't one function.
fn parse_data_icon_shape(value: &Value, json: &Value) -> Result<crate::style::DataIconShape> {
    use crate::style::{DataIconShape, DataIconStep};

    if let Some(name) = value.as_str() {
        return Ok(DataIconShape::Name(name.to_string()));
    }
    let arr = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unsupported \"icon-image\" per-zoom value {value}: {json}"))?;
    match arr.first().and_then(|v| v.as_str()) {
        Some("case") if arr.len() == 4 => {
            // The ONLY test shape in scope is `["==", ["get", key], n]`
            // against a number -- the city labels' `capital == 2` capital
            // marker. Anything else (a `has`, a string comparison, an
            // arithmetic test) is refused rather than approximated: an
            // approximated condition would quietly hand every feature the
            // wrong icon while appearing to work.
            let test = arr[1].as_array().filter(|t| t.first().and_then(|v| v.as_str()) == Some("=="));
            let key = test
                .and_then(|t| t.get(1))
                .and_then(|v| v.as_array())
                .filter(|g| g.first().and_then(|v| v.as_str()) == Some("get"))
                .and_then(|g| g.get(1))
                .and_then(|v| v.as_str());
            let compared = test.and_then(|t| t.get(2)).and_then(|v| v.as_f64());
            let (Some(key), Some(compared)) = (key, compared) else {
                bail!(
                    "a \"case\" in icon-image is only supported as numeric equality against a property \
                     ([\"==\", [\"get\", key], n]): {json}"
                );
            };
            let then_name = arr[2]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("a \"case\" in icon-image needs a string then-branch: {json}"))?
                .to_string();
            Ok(DataIconShape::CaseNumberEq {
                key: key.to_string(),
                value: compared,
                then_name,
                otherwise: Box::new(parse_data_icon_shape(&arr[3], json)?),
            })
        }
        Some("step") => {
            let get_expr = arr
                .get(1)
                .and_then(|v| v.as_array())
                .filter(|g| g.first().and_then(|v| v.as_str()) == Some("get"));
            let key = get_expr
                .and_then(|g| g.get(1))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as icon step input: {json}"))?
                .to_string();
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!("malformed data-driven icon step: {json}");
            }
            let base = arr[2]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("a data-driven icon step's base must be a string name: {json}"))?
                .to_string();
            let mut stops = Vec::new();
            let mut i = 3;
            while i + 1 < arr.len() {
                // The Stage-B trap, refused rather than encoded: this shape
                // picks a bucket by comparing a NUMERIC feature tag against
                // `>=` thresholds, so a step keyed on string labels has no
                // faithful encoding here -- writing one anyway would hand
                // every feature the base bucket while appearing to work.
                // `settlement-subdivision-label`'s `text-size` was left
                // skipped for exactly this reason; see
                // `compile_interpolated_data_step_layout_value`.
                let threshold = arr[i].as_f64().ok_or_else(|| {
                    anyhow::anyhow!(
                        "a data-driven icon step selects a bucket by comparing a NUMERIC feature tag \
                         against `>=` thresholds, so the string threshold {} has no faithful encoding: {json}",
                        arr[i]
                    )
                })?;
                let name = arr[i + 1]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("a data-driven icon step's outputs must be string names: {json}"))?
                    .to_string();
                stops.push((threshold, name));
                i += 2;
            }
            Ok(DataIconShape::Step(DataIconStep { key, base, stops }))
        }
        // `["concat", <part>, ...]` as ONE zoom bracket's sprite name --
        // `poi-label`'s real `icon-image`. Shares `compile_concat_parts`
        // with the top-level `IconImage::Concat` and with `text-field`, so
        // all three agree on what a `concat` part is (in particular on
        // `to-string`'s numeric rule, which is subtle and settled once).
        Some("concat") => Ok(DataIconShape::Concat(compile_concat_parts(&arr[1..], "icon-image", json)?)),
        // The same `["image", name, options?]` expression `compile_icon_image`
        // accepts at the top level, here as ONE zoom bracket's value -- which
        // is exactly where the ten arrow/crosswalk layers put it (`["step",
        // ["zoom"], ["image", "oneway-small", ...], 18, ["image",
        // "oneway-large", ...]]`). Both call sites drop the options through
        // the same `note_dropped_image_options`, so neither can quietly
        // start honouring them without the other.
        Some("image") => {
            let name = arr
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("\"image\" in icon-image needs a name argument: {json}"))?;
            note_dropped_image_options(&arr[1..], json);
            parse_data_icon_shape(name, json)
        }
        // `["match", ["get", key], <labels>, <shape>, ..., <fallback>]` --
        // `tunnel-oneway-arrow-blue`'s z17 bracket. Structurally the same
        // `match` `compile_icon_image` already compiles at the top level,
        // with `DataIconShape` outputs instead of `IconImage` ones; the label
        // parser (`parse_match_labels`) is shared, so the two cannot drift on
        // what a label is.
        Some("match") => {
            // [_, input, (label, output)+, fallback]: at least one pair, and
            // the pairs must come out even once the fallback is set aside.
            if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
                bail!(
                    "\"match\" in a per-zoom icon-image must have the form [\"match\", [\"get\", key], \
                     label1, out1, ..., fallback]: {json}"
                );
            }
            let key = arr[1]
                .as_array()
                .filter(|input| input.first().and_then(|v| v.as_str()) == Some("get"))
                .and_then(|input| input.get(1))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("\"match\" in a per-zoom icon-image must match on [\"get\", key]: {json}")
                })?;
            let mut arms = Vec::new();
            let mut i = 2;
            while i + 1 < arr.len() - 1 {
                arms.push((parse_match_labels(&arr[i], json)?, parse_data_icon_shape(&arr[i + 1], json)?));
                i += 2;
            }
            Ok(DataIconShape::MatchGet {
                key: key.to_string(),
                arms,
                fallback: Box::new(parse_data_icon_shape(arr.last().expect("length checked above"), json)?),
            })
        }
        _ => bail!("unsupported \"icon-image\" per-zoom expression shape: {value}: {json}"),
    }
}

/// `["step", ["get", key], base, t1, v1, ...]`.
///
/// Module-level rather than nested inside `compile_layout_value` because
/// `compile_interpolated_data_step_layout_value` parses exactly the same
/// shape -- once per `interpolate` stop -- and the two must agree on it
/// exactly.
fn parse_data_step(arr: &[Value], json: &Value) -> Result<crate::style::DataStepShape> {
    use crate::style::DataStepShape;
    let get_expr = arr
        .get(1)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as step input: {json}"))?;
    if get_expr.first().and_then(|v| v.as_str()) != Some("get") {
        bail!("expected [\"get\", key] as step input: {json}");
    }
    let key = get_expr
        .get(1)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("\"get\" needs a string key: {json}"))?
        .to_string();
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("malformed data-step shape: {json}");
    }
    let base = arr
        .get(2)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow::anyhow!("step's base value must be numeric: {json}"))? as f32;
    let mut stops = Vec::new();
    let mut i = 3;
    while i + 1 < arr.len() {
        let threshold = arr[i]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("data-step thresholds must be numeric: {json}"))?;
        let value = arr[i + 1]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("data-step stop values must be numeric: {json}"))? as f32;
        stops.push((threshold, value));
        i += 2;
    }
    Ok(DataStepShape { key, base, stops })
}

/// Compiles a zoom-and/or-data-dependent numeric layout property --
/// `text-size`, `icon-size`, and (per-axis) `text-offset`. Scoped to the
/// exact shapes seen in the real vendored styles (see
/// `crate::style::LayoutValue`'s doc comment): a bare number, a zoom-only
/// `["step", ["zoom"], base, t1, v1, ...]`, a data-only `["step", ["get",
/// key], base, t1, v1, ...]`, and the composite `["step", ["zoom"],
/// ["step", ["get", key], ...], t1, ["step", ["get", key], ...], ...]`
/// shape confirmed as poi-label's real `text-size` expression.
///
/// `text-offset` in the real style is a 2-element `[x, y]` pair (see the
/// spec's `["literal", [0, 0]]` example), not a single number -- this
/// compiles ONE number; the caller (`compile_symbol_layer`) calls it twice
/// against the unwrapped x and y sub-expressions of the offset pair.
///
/// Also handles a zoom `interpolate` curve, by delegating to the paint
/// side's existing curve compiler -- see
/// `compile_zoom_interpolate_layout_value` -- including the composite case
/// where the curve's own stop outputs are per-feature `step` expressions,
/// which lands on `LayoutValue::ZoomOfDataStep` just like the `step`-of-
/// `step` shape does.
pub(crate) fn compile_layout_value(json: &Value) -> Result<crate::style::LayoutValue> {
    use crate::style::LayoutValue;

    if let Some(n) = json.as_f64() {
        return Ok(LayoutValue::Constant(n as f32));
    }
    let arr = json
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("layout value must be a number or expression array: {json}"))?;
    // A zoom `interpolate` curve -- real examples in the vendored Mapbox
    // Streets style: `crosswalks`' `icon-size`, `road-intersection`'s and
    // `continent-label`'s `text-size`. Checked BEFORE the `step`-only
    // rejection below, and delegated wholesale to the paint side's curve
    // compiler rather than re-derived here.
    if arr.first().and_then(|v| v.as_str()) == Some("interpolate") {
        return compile_zoom_interpolate_layout_value(json);
    }
    if arr.first().and_then(|v| v.as_str()) != Some("step") {
        bail!("unsupported layout value expression shape: {json}");
    }
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"step\" expression must have the form [\"step\", input, base, t1, v1, ...]: {json}");
    }
    let input = arr
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("\"step\" needs an input expression: {json}"))?;
    let input_kind = input.as_array().and_then(|a| a.first()).and_then(|v| v.as_str());
    match input_kind {
        Some("zoom") => {
            let base_json = &arr[2];
            // Composite shape: the base itself is a nested ["step", ["get",
            // key], ...] -- detect this BEFORE trying to parse the base as a
            // plain number (a naive `as_f64()` on a nested array silently
            // fails, masking the composite case as "base must be numeric").
            if let Some(base_arr) = base_json.as_array() {
                if base_arr.first().and_then(|v| v.as_str()) == Some("step")
                    && base_arr.get(1).and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str())
                        == Some("get")
                {
                    let mut zoom_stops = vec![(0u8, parse_data_step(base_arr, json)?)];
                    let mut i = 3;
                    while i + 1 < arr.len() {
                        let zoom = arr[i]
                            .as_f64()
                            .ok_or_else(|| anyhow::anyhow!("step zoom stops must be numeric: {json}"))?
                            as u8;
                        let inner = arr[i + 1]
                            .as_array()
                            .ok_or_else(|| anyhow::anyhow!("expected a nested data-step at each zoom stop: {json}"))?;
                        zoom_stops.push((zoom, parse_data_step(inner, json)?));
                        i += 2;
                    }
                    return Ok(LayoutValue::ZoomOfDataStep(zoom_stops));
                }
            }
            // Plain zoom-only step.
            let base = base_json
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("step's base value must be numeric: {json}"))? as f32;
            let mut stops = vec![(0u8, base)];
            let mut i = 3;
            while i + 1 < arr.len() {
                let zoom = arr[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("step zoom stops must be numeric: {json}"))? as u8;
                let value = arr[i + 1]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("step stop values must be numeric: {json}"))? as f32;
                stops.push((zoom, value));
                i += 2;
            }
            Ok(LayoutValue::ZoomStep(ZoomValue::stepped(stops)))
        }
        Some("get") => Ok(LayoutValue::DataStep(parse_data_step(arr, json)?)),
        _ => bail!("unsupported \"step\" input for layout value: {json}"),
    }
}

/// Compiles a zoom `interpolate` layout value (`["interpolate", <type>,
/// ["zoom"], z1, v1, ...]`) by handing the raw JSON straight to
/// `expression::compile_zoom_value` -- the SAME full curve compiler the
/// paint side already uses for `line-width`/`fill-color`/etc. That compiler
/// already implements both real interpolation types (`linear` and
/// `exponential` with its declared base), already resamples the true curve
/// at every integer zoom in the stops' range (so fractional stop zooms like
/// `continent-label`'s `2.5` survive `ZoomValue`'s `u8` stop keys), and
/// already rejects property-driven (`["get", ...]`-input) interpolation.
/// Reusing it, rather than re-deriving the sampling here, is what keeps
/// layout and paint values agreeing about what a given curve means.
///
/// That delegation only covers an `interpolate` whose stop outputs are plain
/// numbers. When they are themselves expressions, the curve varies per
/// FEATURE as well as per zoom, which `compile_zoom_value` would expand into
/// several filtered `(Filter, ZoomValue)` pairs -- and `LayoutValue` carries
/// no per-case filter. Those go to
/// `compile_interpolated_data_step_layout_value` instead, which represents
/// the shape directly as a `LayoutValue::ZoomOfDataStep`.
///
/// Known, deliberate limitation: `LayoutValue::resolve` takes an INTEGER
/// zoom, and symbol plans are rebaked per zoom bucket, so an interpolated
/// text/icon size STEPS at each integer zoom rather than growing
/// continuously as the camera zooms between them. That is exactly how every
/// already-compiled paint-side `interpolate` behaves in this crate (see
/// `expression::compile_interpolate`'s resampling doc comment); making it
/// continuous is a renderer-side concern, not a compiler-side one.
fn compile_zoom_interpolate_layout_value(json: &Value) -> Result<crate::style::LayoutValue> {
    let arr = json.as_array().expect("the caller only routes array-shaped JSON here");
    // A stop OUTPUT that is an expression array rather than a bare number
    // means the curve varies per feature too -- `state-label`'s and
    // `country-label`'s `["step", ["get", "symbolrank"], ...]` stops, and
    // `settlement-subdivision-label`'s `["match", ["get", "type"], ...]`
    // ones. Detected here, up front, rather than by letting
    // `compile_zoom_value` fail on them, because only one of those two
    // shapes even reaches it: `number_leaf` rejects a `step` output outright
    // ("expected a number"), while a `match` output takes the paint side's
    // jointly-varying path and comes back as several filtered cases. Same
    // structural situation, two unrelated symptoms.
    let stop_outputs_are_expressions = arr.get(3..).unwrap_or(&[]).iter().skip(1).step_by(2).any(Value::is_array);
    if stop_outputs_are_expressions {
        return compile_interpolated_data_step_layout_value(arr, json).or_else(|error| {
            // Stops that are ARITHMETIC over properties (`water-point-label`'s
            // `["*", ["-", 16, ["sqrt", ["get", "sizerank"]]], 1]`) have no
            // bucket encoding; keep the expression and evaluate it per
            // feature at bake time. Only when every operator in it is one
            // the evaluator handles per feature -- otherwise the bucket
            // compiler's own error stands.
            let zoom_input = arr.get(2).and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str)
                == Some("zoom");
            // Scoped to ARITHMETIC (and property-`interpolate`) stops: a stop list of `step`s/`match`es
            // the bucket compiler refused (mismatched keys, disagreeing
            // label groups) keeps its refusal, as its tests pin.
            let arithmetic_stop = arr.get(3..).unwrap_or(&[]).iter().skip(1).step_by(2).any(|stop| {
                matches!(
                    stop.as_array().and_then(|a| a.first()).and_then(Value::as_str),
                    Some(
                        "*" | "-" | "+" | "/" | "%" | "sqrt" | "abs" | "min" | "max" | "to-number" | "coalesce" | "get"
                            // A stop that is itself a PROPERTY `interpolate`
                            // (Standard's `water line label`) is continuous
                            // in the property, so no bucket list holds it
                            // either.
                            | "interpolate"
                    )
                )
            });
            if zoom_input && arithmetic_stop && expression::is_per_feature_evaluable(json) {
                Ok(crate::style::LayoutValue::FeatureExpression {
                    expr: std::sync::Arc::new(json.clone()),
                    // Overridden per property by `with_fallback` where the
                    // caller knows the property's style-spec default.
                    fallback: 0.0,
                    blend: true,
                })
            } else {
                Err(error)
            }
        });
    }

    let mut cases: Vec<(Filter, ZoomValue<f32>)> = compile_zoom_value(json, number_leaf, Scope::root())?;
    if cases.len() != 1 || !matches!(cases[0].0, Filter::Any) {
        bail!(
            "property-varying \"interpolate\" layout value (it compiled to {} filtered case(s) \
             rather than a single unconditional zoom curve) is not representable by \
             `LayoutValue`, which carries no per-case filter: {json}",
            cases.len()
        );
    }
    let (_, curve) = cases.pop().expect("length checked to be exactly 1 above");
    Ok(crate::style::LayoutValue::ZoomStep(curve))
}

/// Compiles an `interpolate` over `["zoom"]` whose stop OUTPUTS are
/// themselves per-feature expressions -- a curve that varies by zoom AND by
/// feature at once -- into `LayoutValue::ZoomOfDataStep` (stops are
/// `["step", ["get", key], ...]`, a NUMERIC bucket) or
/// `LayoutValue::ZoomOfMatchStep` (stops are `["match", ["get", key], ...]`,
/// a STRING-label bucket), whichever shape every stop agrees on. Both are
/// exactly "at each zoom, a per-feature bucket rule on a property".
///
/// Real inputs (verbatim, from `examples/mapbox-streets-style.json`):
///   `state-label`'s `text-size`:
///   `["interpolate", ["cubic-bezier", 0.85, 0.7, 0.65, 1], ["zoom"],
///     4, ["step", ["get", "symbolrank"], 9, 6, 8, 7, 7],
///     9, ["step", ["get", "symbolrank"], 21, 6, 16, 7, 14]]`
///   `country-label`'s `text-size`, the same shape with the bezier
///   `[0.2, 0, 0.7, 1]`, zooms 1/9 and buckets on symbolrank 4/5.
///
///   `settlement-major-label`'s `text-size`, the same shape with the bezier
///   `[0.2, 0, 0.9, 1]`, four zoom stops (3/6/8/15) and symbolrank bucket
///   sets that DIFFER from stop to stop: `[6]`, `[6, 7]`, `[9, 10]`,
///   `[9, 12, 15]`. `settlement-minor-label` likewise, with the nested sets
///   `[9]`, `[9, 12]`, `[9, 12, 15]`, `[9, 12, 15]`.
///
/// Sampled the same way the paint side samples every other `interpolate`:
/// at every integer zoom in `[floor(first_stop), ceil(last_stop)]`, with the
/// value at each zoom being the two bracketing stops' shapes mixed along the
/// declared curve (`expression::Interpolation`, shared verbatim with the
/// paint compiler so both sides agree on what a curve means). Mixing two
/// `DataStepShape`s means mixing them BUCKET BY BUCKET: each zoom's shape
/// keeps the shared thresholds and interpolates the value inside each one.
///
/// "The corresponding bucket in the other stop" needs the stops to share a
/// bucket structure, which the real city labels above plainly do not. They
/// are made to, losslessly, BEFORE any mixing happens: every stop is
/// re-expressed on the UNION of all the stops' thresholds by
/// `refine_data_step_onto_thresholds`, which cannot change what any stop
/// returns for any input (a superset threshold merely restates the value
/// already in effect there -- see that function, and the test that proves
/// it over a full sweep of property values). What the union does NOT do is
/// paper over a real disagreement: it only ever adds bucket boundaries, and
/// each stop keeps deciding its own value at each of them.
///
/// PRECONDITION, enforced rather than assumed: the stops must agree on the
/// property KEY. That one is not reconcilable -- a `symbolrank` bucket and a
/// `sizerank` bucket have no correspondence to refine onto -- so a
/// disagreement is an `Err`, i.e. a loud per-layer skip. All the real layers
/// satisfy it.
///
/// `["match", ["get", key], ...]`-shaped stop outputs ARE representable, as
/// `LayoutValue::ZoomOfMatchStep` -- see `MatchStepShape`'s doc comment and
/// `compile_match_step_stops` below. What stays refused is anything neither
/// shape can carry: an arithmetic stop output (`water-point-label`'s real
/// `text-size`, `["*", ["-", 16, ["sqrt", ["get", "sizerank"]]], 1]`), or
/// stops that mix `step` and `match` outputs, or `match` stops whose label
/// GROUPS don't correspond one-for-one across stops (see
/// `compile_match_step_stops`'s own precondition). Encoding any of those as
/// a `DataStepShape`/`MatchStepShape` anyway would not be merely lossy, it
/// would hand every feature the wrong bucket while looking like it worked,
/// so those layers keep soft-skipping instead.
fn compile_interpolated_data_step_layout_value(arr: &[Value], json: &Value) -> Result<crate::style::LayoutValue> {
    use expression::Interpolation;

    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"interpolate\" expression must have the form [\"interpolate\", [type,...], [\"zoom\"], z1, v1, z2, v2, ...]: {json}");
    }
    let curve = Interpolation::parse(&arr[1], json)?;
    let is_zoom_input = arr[2].as_array().is_some_and(|z| z.first().and_then(Value::as_str) == Some("zoom"));
    if !is_zoom_input {
        bail!("\"interpolate\" expression's third element must be [\"zoom\"] (property-based interpolation is not supported): {json}");
    }

    // Every stop must be one of the two per-feature shapes `LayoutValue`
    // knows, and every stop must be the SAME one of the two -- peeking at
    // the FIRST stop's head decides which parser the rest are held to,
    // exactly as the key-agreement check below holds every `step` stop to
    // the first stop's property.
    let first_output = arr.get(4).and_then(Value::as_array).ok_or_else(|| anyhow::anyhow!(
        "every stop of a property-varying \"interpolate\" layout value must itself be a \
         [\"step\", [\"get\", key], ...] or [\"match\", [\"get\", key], ...] expression, but the \
         first stop is the bare value {}: {json}",
        arr[4]
    ))?;
    match first_output.first().and_then(Value::as_str) {
        Some("step") => compile_data_step_stops(arr, json, curve),
        Some("match") => compile_match_step_stops(arr, json, curve),
        _ => bail!(
            "property-varying \"interpolate\" layout value whose stop outputs are {} expressions is \
             not representable: the only per-feature shapes `LayoutValue` has are `DataStepShape` \
             (a [\"step\", [\"get\", key], ...] that picks a bucket by comparing a NUMERIC feature \
             tag against `>=` thresholds) and `MatchStepShape` (a [\"match\", [\"get\", key], ...] \
             that picks a bucket by STRING-label equality) -- so an arithmetic expression has no \
             faithful encoding there: {json}",
            first_output.first().and_then(Value::as_str).map(|s| format!("{s:?}")).unwrap_or_else(|| "unrecognized".into())
        ),
    }
}

/// The `["step", ["get", key], ...]`-stopped half of
/// `compile_interpolated_data_step_layout_value` -- unchanged behaviour,
/// factored out so the dispatch above can hand it just the `DataStepShape`
/// stops. See that function's doc comment for the shape and the real
/// `state-label`/`country-label`/`settlement-*-label` inputs.
fn compile_data_step_stops(arr: &[Value], json: &Value, curve: expression::Interpolation) -> Result<crate::style::LayoutValue> {
    use crate::style::{DataStepShape, LayoutValue};

    let mut stops: Vec<(f64, DataStepShape)> = Vec::new();
    let mut i = 3;
    while i + 1 < arr.len() {
        let zoom = arr[i].as_f64().ok_or_else(|| anyhow::anyhow!("\"interpolate\" stop zoom must be a number: {json}"))?;
        let output = arr[i + 1].as_array().ok_or_else(|| anyhow::anyhow!(
            "every stop of a property-varying \"interpolate\" layout value must itself be a \
             [\"step\", [\"get\", key], ...] expression, but one stop is the bare value {}: {json}",
            arr[i + 1]
        ))?;
        match output.first().and_then(Value::as_str) {
            Some("step") => stops.push((zoom, parse_data_step(output, json)?)),
            _ => bail!(
                "a property-varying \"interpolate\" layout value's stops must all be the SAME \
                 per-feature shape, but the first stop is [\"step\", ...] and this one is {} -- \
                 mixing them has no defensible bucket correspondence: {json}",
                output.first().and_then(Value::as_str).map(|s| format!("{s:?}")).unwrap_or_else(|| "unrecognized".into())
            ),
        }
        i += 2;
    }

    // The one precondition that survives: every stop must step on the SAME
    // property. Differing thresholds can be reconciled (below); differing
    // KEYS cannot -- a `symbolrank` bucket and a `sizerank` bucket have no
    // correspondence at all, and inventing one would be a guess.
    let (_, reference) = stops.first().expect("arr.len() checked above guarantees at least one stop");
    for (zoom, shape) in &stops[1..] {
        if shape.key != reference.key {
            bail!(
                "a property-varying \"interpolate\" layout value's stops must all step on the SAME \
                 property, but the first steps on {:?} and the z{zoom} stop on {:?}; there is no \
                 defensible way to pair up their buckets: {json}",
                reference.key,
                shape.key
            );
        }
    }

    // Align the stops onto the UNION of their thresholds so that "the
    // corresponding bucket in the other stop" is always defined. The real
    // city labels need this: `settlement-major-label`'s four zoom stops
    // declare [6], [6,7], [9,10] and [9,12,15]. Refinement is lossless --
    // an added threshold restates the value already in effect there, so no
    // stop's behaviour changes for any input; see
    // `refine_data_step_onto_thresholds`. Only the WRITING changes, and it
    // has to, because interpolation is defined bucket by bucket.
    let mut union: Vec<f64> = stops.iter().flat_map(|(_, shape)| shape.stops.iter().map(|(t, _)| *t)).collect();
    union.sort_by(|a, b| a.partial_cmp(b).expect("thresholds are parsed from JSON numbers, never NaN"));
    // Same 1e-9 tolerance the old equal-thresholds check used, so two stops
    // that name the same threshold with different float spellings still
    // share one bucket rather than growing two adjacent, near-identical ones.
    union.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let stops: Vec<(f64, DataStepShape)> =
        stops.iter().map(|(zoom, shape)| (*zoom, refine_data_step_onto_thresholds(shape, &union))).collect();

    let first_zoom = stops.first().expect("at least one stop, checked above").0;
    let last_zoom = stops.last().expect("at least one stop, checked above").0;
    let start = first_zoom.floor() as i64;
    let end = last_zoom.ceil() as i64;
    let sampled =
        (start..=end).map(|z| (z as u8, data_step_at(&stops, z as f64, curve))).collect();
    Ok(LayoutValue::ZoomOfDataStep(sampled))
}

/// The `["match", ["get", key], ...]`-stopped half of
/// `compile_interpolated_data_step_layout_value`: the real
/// `road-label`/`path-pedestrian-label`/`settlement-subdivision-label`
/// `text-size` shape, e.g. `road-label`'s
/// `["interpolate", ["linear"], ["zoom"],
///   10, ["match", ["get", "class"],
///        ["motorway","trunk","primary","secondary","tertiary"], 10,
///        ["motorway_link","trunk_link","primary_link","secondary_link",
///         "tertiary_link","street","street_limited"], 9,
///        6.5],
///   18, ["match", ["get", "class"], <same two label groups>, 16, 14, 13]]`.
///
/// Real MapLibre evaluates an `interpolate` whose stop outputs are
/// expressions by evaluating BOTH bracketing stops' full sub-expressions for
/// the CURRENT feature, then blending the two resulting numbers by the zoom
/// curve (`Interpolate.evaluate` in `@maplibre/maplibre-gl-style-spec`,
/// `src/expression/definitions/interpolate.ts`: `outputLower =
/// outputs[index].evaluate(ctx)`, `outputUpper = outputs[index +
/// 1].evaluate(ctx)`, then `interpolate[type](outputLower, outputUpper, t)`).
/// For a `match` sub-expression that means: evaluate the match at each
/// bracketing stop (i.e. find which arm the feature's tag falls in, at EACH
/// stop independently) and mix those two numbers.
///
/// This function bakes that into the same precomputed-per-integer-zoom
/// table `compile_data_step_stops` already uses for `ZoomOfDataStep`, for
/// the same reason: `LayoutValue::resolve` takes an integer zoom and a
/// symbol plan is rebaked per zoom bucket, so the curve is sampled once at
/// every integer zoom in range rather than evaluated fresh per frame. Doing
/// that for a `match` needs "the corresponding arm in the other stop" to be
/// defined, which -- unlike a numeric `step`'s thresholds -- cannot be made
/// true by adding more of them: a label-equality partition has no natural
/// coarser/finer relationship between two different partitions to refine
/// onto. So this function does not reconcile disagreeing arms the way
/// `compile_data_step_stops` reconciles disagreeing thresholds; it requires
/// every stop to declare the SAME key and the SAME sequence of label GROUPS
/// (compared as sets, so re-ordering the labels within one arm is fine, but
/// splitting/merging/reordering the ARMS is not) and refuses otherwise. All
/// three real layers satisfy this: every stop in each one declares identical
/// label groups, only the numbers differ.
fn compile_match_step_stops(arr: &[Value], json: &Value, curve: expression::Interpolation) -> Result<crate::style::LayoutValue> {
    use crate::style::{LayoutValue, MatchStepShape};

    let mut stops: Vec<(f64, MatchStepShape)> = Vec::new();
    let mut i = 3;
    while i + 1 < arr.len() {
        let zoom = arr[i].as_f64().ok_or_else(|| anyhow::anyhow!("\"interpolate\" stop zoom must be a number: {json}"))?;
        let output = arr[i + 1].as_array().ok_or_else(|| anyhow::anyhow!(
            "every stop of a property-varying \"interpolate\" layout value must itself be a \
             [\"match\", [\"get\", key], ...] expression, but one stop is the bare value {}: {json}",
            arr[i + 1]
        ))?;
        match output.first().and_then(Value::as_str) {
            Some("match") => stops.push((zoom, parse_match_step(output, json)?)),
            _ => bail!(
                "a property-varying \"interpolate\" layout value's stops must all be the SAME \
                 per-feature shape, but the first stop is [\"match\", ...] and this one is {} -- \
                 mixing them has no defensible bucket correspondence: {json}",
                output.first().and_then(Value::as_str).map(|s| format!("{s:?}")).unwrap_or_else(|| "unrecognized".into())
            ),
        }
        i += 2;
    }

    // PRECONDITION 1: every stop must match on the SAME property, exactly
    // the `step` path's key-agreement rule.
    let (_, reference) = stops.first().expect("arr.len() checked above guarantees at least one stop");
    for (zoom, shape) in &stops[1..] {
        if shape.key != reference.key {
            bail!(
                "a property-varying \"interpolate\" layout value's stops must all match on the SAME \
                 property, but the first matches on {:?} and the z{zoom} stop on {:?}; there is no \
                 defensible way to pair up their arms: {json}",
                reference.key,
                shape.key
            );
        }
    }

    // PRECONDITION 2: every stop must declare the SAME sequence of label
    // GROUPS (order-independent within a group), so "the corresponding arm
    // in the other stop" is well-defined without inventing a merge. See this
    // function's doc comment for why no union/refinement trick applies here
    // the way it does for numeric thresholds.
    let reference_groups: Vec<std::collections::BTreeSet<&str>> =
        reference.arms.iter().map(|(labels, _)| labels.iter().map(String::as_str).collect()).collect();
    for (zoom, shape) in &stops[1..] {
        let groups: Vec<std::collections::BTreeSet<&str>> =
            shape.arms.iter().map(|(labels, _)| labels.iter().map(String::as_str).collect()).collect();
        if groups != reference_groups {
            bail!(
                "a property-varying \"interpolate\" layout value's \"match\" stops must all declare \
                 the SAME label groups in the SAME order, but the z{zoom} stop's groups differ from \
                 the first stop's; there is no lossless way to reconcile two different label \
                 partitions: {json}",
            );
        }
    }

    let first_zoom = stops.first().expect("at least one stop, checked above").0;
    let last_zoom = stops.last().expect("at least one stop, checked above").0;
    let start = first_zoom.floor() as i64;
    let end = last_zoom.ceil() as i64;
    let sampled = (start..=end).map(|z| (z as u8, match_step_at(&stops, z as f64, curve))).collect();
    Ok(LayoutValue::ZoomOfMatchStep(sampled))
}

/// `["step", ["zoom"], <bracket>, z1, <bracket>, ...]` where each bracket
/// is a number or a `["match", ["get", key], n1, out1, ..., default]` over
/// NUMERIC labels and outputs -- Mapbox Streets' settlement
/// `text-radial-offset`, `["step", ["zoom"], ["match", ["get", "capital"],
/// 2, 0.6, 0.55], 8, 0]`.
///
/// Compiled to `LayoutValue::ZoomOfDataStep`, EXACTLY: equality on a
/// number is a step up to its output AT the label and back down to the
/// default at the very next `f64` (`next_up`), so `n == label` is the only
/// input that lands on the arm. Used only where the caller has already
/// failed `compile_layout_value`, so it changes no other property's shape.
fn compile_zoom_of_numeric_match(json: &Value) -> Result<crate::style::LayoutValue> {
    use crate::style::DataStepShape;

    fn bracket(value: &Value, json: &Value) -> Result<DataStepShape> {
        if let Some(n) = value.as_f64() {
            return Ok(DataStepShape { key: String::new(), base: n as f32, stops: Vec::new() });
        }
        let arr = value
            .as_array()
            .filter(|a| a.first().and_then(Value::as_str) == Some("match"))
            .ok_or_else(|| anyhow::anyhow!("expected a number or a numeric \"match\": {json}"))?;
        let key = arr
            .get(1)
            .and_then(Value::as_array)
            .filter(|g| g.len() == 2 && g.first().and_then(Value::as_str) == Some("get"))
            .and_then(|g| g[1].as_str())
            .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as \"match\" input: {json}"))?
            .to_string();
        if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
            bail!("malformed numeric match: {json}");
        }
        let default = arr[arr.len() - 1]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("\"match\" default output must be numeric: {json}"))? as f32;
        let mut arms = Vec::new();
        let mut i = 2;
        while i + 1 < arr.len() - 1 {
            let output = arr[i + 1]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("\"match\" output must be numeric: {json}"))? as f32;
            let labels: Vec<f64> = match &arr[i] {
                Value::Array(list) => list.iter().map(Value::as_f64).collect::<Option<_>>(),
                single => single.as_f64().map(|n| vec![n]),
            }
            .filter(|l: &Vec<f64>| !l.is_empty() && l.iter().all(|n| n.is_finite()))
            .ok_or_else(|| anyhow::anyhow!("only numeric \"match\" labels are read here: {json}"))?;
            arms.extend(labels.into_iter().map(|label| (label, output)));
            i += 2;
        }
        arms.sort_by(|a, b| a.0.total_cmp(&b.0));
        if arms.windows(2).any(|w| w[0].0 == w[1].0) {
            bail!("a \"match\" label appears twice: {json}");
        }
        let mut stops = Vec::new();
        for (label, output) in arms {
            stops.push((label, output));
            stops.push((label.next_up(), default));
        }
        Ok(DataStepShape { key, base: default, stops })
    }

    let arr = json.as_array().ok_or_else(|| anyhow::anyhow!("expected an expression: {json}"))?;
    let is_zoom = arr.get(1).and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) == Some("zoom");
    if arr.first().and_then(Value::as_str) != Some("step") || !is_zoom || arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("expected [\"step\", [\"zoom\"], ...]: {json}");
    }
    let mut brackets = vec![(0u8, bracket(&arr[2], json)?)];
    let mut i = 3;
    while i + 1 < arr.len() {
        let zoom = arr[i].as_f64().ok_or_else(|| anyhow::anyhow!("step zoom stops must be numeric: {json}"))? as u8;
        brackets.push((zoom, bracket(&arr[i + 1], json)?));
        i += 2;
    }
    Ok(crate::style::LayoutValue::ZoomOfDataStep(brackets))
}

/// `["match", ["get", key], labels1, out1, ..., default]` over NUMERIC
/// outputs -- the stop shape `compile_match_step_stops` parses. The
/// string-keyed sibling of `parse_data_step`; reuses `parse_match_labels`
/// (already shared with `icon-image`'s `MatchGet` parsing) for the label
/// side of each arm.
fn parse_match_step(arr: &[Value], json: &Value) -> Result<crate::style::MatchStepShape> {
    use crate::style::MatchStepShape;

    let key = arr
        .get(1)
        .and_then(|v| v.as_array())
        .filter(|g| g.first().and_then(|v| v.as_str()) == Some("get"))
        .and_then(|g| g.get(1))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as \"match\" input: {json}"))?
        .to_string();
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("malformed match-step shape: {json}");
    }
    let mut arms = Vec::new();
    let mut i = 2;
    while i + 1 < arr.len() - 1 {
        let labels = parse_match_labels(&arr[i], json)?;
        let value = arr[i + 1]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("\"match\" stop output must be numeric: {json}"))? as f32;
        arms.push((labels, value));
        i += 2;
    }
    let default = arr
        .last()
        .expect("length checked above guarantees at least the default element")
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("\"match\" default output must be numeric: {json}"))? as f32;
    Ok(MatchStepShape { key, arms, default })
}

/// The `MatchStepShape` analogue of `data_step_at`: mixes the two stops
/// bracketing zoom `z` ARM BY ARM along `curve`, clamping to the first/last
/// stop outside their range. Callers must have checked that every stop
/// shares one key and one sequence of label groups (see
/// `compile_match_step_stops`).
fn match_step_at(
    stops: &[(f64, crate::style::MatchStepShape)],
    z: f64,
    curve: expression::Interpolation,
) -> crate::style::MatchStepShape {
    use crate::style::{Lerp, MatchStepShape};

    let first = &stops[0];
    if z <= first.0 {
        return first.1.clone();
    }
    let last = &stops[stops.len() - 1];
    if z >= last.0 {
        return last.1.clone();
    }
    let index = stops
        .iter()
        .rposition(|(stop_zoom, _)| *stop_zoom <= z)
        .expect("z is strictly between the first and last stop (checked above), so a lower bracket exists");
    let (za, low) = &stops[index];
    let (zb, high) = &stops[index + 1];
    let t = curve.t_at(z, *za, *zb) as f32;
    MatchStepShape {
        key: low.key.clone(),
        arms: low
            .arms
            .iter()
            .zip(&high.arms)
            .map(|((labels, low_value), (_, high_value))| (labels.clone(), low_value.lerp(high_value, t)))
            .collect(),
        default: low.default.lerp(&high.default, t),
    }
}

/// Re-expresses `shape`'s step on `thresholds`, which MUST be a superset of
/// `shape`'s own thresholds, sorted ascending.
///
/// This is exact, not an approximation, and that is the whole load-bearing
/// claim behind interpolating stops whose bucket thresholds disagree. A
/// `step` is a piecewise-constant function of the property value; adding a
/// threshold the shape did not declare simply starts a new bucket carrying
/// the value that was ALREADY in effect at that point, so the function's
/// graph is unchanged -- only the number of segments it is written in.
/// Formally: for input `x`, the original yields the value of the greatest
/// declared threshold `<= x` (or `base` if there is none); the refined shape
/// yields the value at the greatest UNION threshold `u <= x`, and since no
/// declared threshold lies in `(u, x]`, that value is by construction the
/// same one. `base` is untouched because it is the "below every threshold"
/// case, and a superset adds no threshold below the smallest existing one
/// that could steal it.
///
/// Proved rather than asserted, by
/// `refining_a_data_step_onto_a_superset_of_its_thresholds_resolves_identically_for_every_input`.
fn refine_data_step_onto_thresholds(
    shape: &crate::style::DataStepShape,
    thresholds: &[f64],
) -> crate::style::DataStepShape {
    use crate::style::DataStepShape;
    // Deliberately mirrors `DataStepShape::resolve`'s own bucket search
    // (last threshold `<= x`, else `base`) so the refined shape is defined
    // by the very rule it will later be read back with.
    let value_at = |x: f64| {
        shape
            .stops
            .iter()
            .rev()
            .find(|(threshold, _)| x >= *threshold)
            .map(|(_, v)| *v)
            .unwrap_or(shape.base)
    };
    DataStepShape {
        key: shape.key.clone(),
        base: shape.base,
        stops: thresholds.iter().map(|t| (*t, value_at(*t))).collect(),
    }
}

/// The `DataStepShape` analogue of `expression::interpolate_at`: mixes the
/// two stops bracketing zoom `z` bucket by bucket along `curve`, clamping to
/// the first/last stop outside their range exactly as the style spec does.
/// Callers must have checked that every stop shares one key, and must have
/// put them all on one common threshold list first (see
/// `compile_interpolated_data_step_layout_value` and
/// `refine_data_step_onto_thresholds`).
fn data_step_at(
    stops: &[(f64, crate::style::DataStepShape)],
    z: f64,
    curve: expression::Interpolation,
) -> crate::style::DataStepShape {
    use crate::style::{DataStepShape, Lerp};

    let first = &stops[0];
    if z <= first.0 {
        return first.1.clone();
    }
    let last = &stops[stops.len() - 1];
    if z >= last.0 {
        return last.1.clone();
    }
    let index = stops
        .iter()
        .rposition(|(stop_zoom, _)| *stop_zoom <= z)
        .expect("z is strictly between the first and last stop (checked above), so a lower bracket exists");
    let (za, low) = &stops[index];
    let (zb, high) = &stops[index + 1];
    let t = curve.t_at(z, *za, *zb) as f32;
    DataStepShape {
        key: low.key.clone(),
        base: low.base.lerp(&high.base, t),
        stops: low
            .stops
            .iter()
            .zip(&high.stops)
            .map(|((threshold, low_value), (_, high_value))| (*threshold, low_value.lerp(high_value, t)))
            .collect(),
    }
}

/// Compiles a zoom-and/or-data-dependent `[x, y]` *pair* layout property --
/// specifically `text-offset`'s real "step-of-literal-pairs" shape,
/// confirmed as `poi-label`'s real `text-offset` expression:
/// `step(["zoom"], step(["get","sizerank"], ["literal",[0,0]], 5,
/// ["literal",[0,0.8]]), 17, step(["get","sizerank"], ["literal",[0,0]], 13,
/// ["literal",[0,0.8]]))`.
///
/// Recognizes the exact same shapes `compile_layout_value` does (a
/// constant, a zoom-only step, a data-only step, and the composite
/// zoom-of-data-step), except every leaf/base value here is an
/// `["literal", [x, y]]` pair instead of a bare number. Returns TWO
/// independent `LayoutValue`s (one per axis) rather than inventing a new
/// pair-valued type -- this matches exactly what `StyleLayer::Symbol`'s
/// existing `text_offset_x`/`text_offset_y: LayoutValue` fields expect.
/// Compiles `layout.text-anchor` into a `TextAnchorValue`.
///
/// Three shapes, which is every one either vendored style declares:
///
/// - a bare keyword (`"top"`, `"left"`, ...);
/// - `["step", ["get", key], base, t1, v1, ...]`, a keyword chosen by a
///   numeric feature property;
/// - `["step", ["zoom"], <bracket>, z1, <bracket>, ...]` where each bracket
///   is either of the two above -- Mapbox Standard's `poi-label` and
///   `natural-point-label`, whose `sizerank` threshold for "hang the label
///   below the icon" tightens from 5 to 13 at z17.
///
/// The zoom `step` WRAPPING a data `step` is the shape that was missing, and
/// its absence is the whole of the "text sits on top of its own icon" defect:
/// with the expression unreadable the property degraded to the spec default
/// of `center`, and `text-offset` -- a ramp over the same `sizerank` --
/// degraded at the same moment, so nothing was left to move the label off
/// the sprite.
///
/// Returns `Err` for anything else (Mapbox Streets' `["match", ["get",
/// "stop_type"], ...]` on five layers, still out of scope), which the caller
/// degrades to `center` rather than skipping the layer over.
fn compile_text_anchor(json: &Value) -> Result<crate::style::TextAnchorValue> {
    use crate::style::{DataAnchorStep, TextAnchor, TextAnchorValue};

    /// One keyword, validated as the style spec's own.
    fn keyword(json: &Value) -> Result<TextAnchor> {
        json.as_str()
            .and_then(TextAnchor::from_keyword)
            .ok_or_else(|| anyhow::anyhow!("text-anchor must be a style-spec anchor keyword: {json}"))
    }

    /// A whole zoom bracket: a bare keyword (a step with no stops) or
    /// `["step", ["get", key], ...]` over keywords.
    fn bracket(json: &Value, whole: &Value) -> Result<DataAnchorStep> {
        let Some(arr) = json.as_array() else {
            return Ok(DataAnchorStep { key: String::new(), base: keyword(json)?, stops: Vec::new() });
        };
        // `["get", key]`: the feature carries its anchor KEYWORD as a tag.
        // Mapbox Streets' `settlement-major-label`/`-minor-label` below z8
        // (`["step", ["zoom"], ["get", "text_anchor"], 8, "center"]`): the
        // tileset has already chosen which side of the town dot each name
        // goes on. Unreadable, this degraded the whole property to
        // `center` and every city name sat centred ON its dot.
        if arr.first().and_then(Value::as_str) == Some("get") {
            let key = arr
                .get(1)
                .and_then(Value::as_str)
                .filter(|_| arr.len() == 2)
                .ok_or_else(|| anyhow::anyhow!("\"get\" needs exactly one string key: {whole}"))?;
            return Ok(DataAnchorStep { key: key.to_string(), base: TextAnchor::Center, stops: Vec::new() });
        }
        if arr.first().and_then(Value::as_str) != Some("step") {
            bail!("a text-anchor zoom bracket must be a keyword, [\"get\", key] or a [\"step\", [\"get\", key], ...]: {whole}");
        }
        let get = arr
            .get(1)
            .and_then(Value::as_array)
            .filter(|g| g.first().and_then(Value::as_str) == Some("get"))
            .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as text-anchor step input: {whole}"))?;
        let key = get
            .get(1)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("\"get\" needs a string key: {whole}"))?
            .to_string();
        if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
            bail!("malformed text-anchor data step: {whole}");
        }
        let base = keyword(&arr[2])?;
        let mut stops = Vec::new();
        let mut i = 3;
        while i + 1 < arr.len() {
            let threshold = arr[i]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("text-anchor step thresholds must be numeric: {whole}"))?;
            stops.push((threshold, keyword(&arr[i + 1])?));
            i += 2;
        }
        stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(DataAnchorStep { key, base, stops })
    }

    let Some(arr) = json.as_array() else {
        return Ok(TextAnchorValue::Constant(keyword(json)?));
    };
    if arr.first().and_then(Value::as_str) != Some("step") {
        bail!("unsupported text-anchor expression shape: {json}");
    }
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"step\" expression must have the form [\"step\", input, base, t1, v1, ...]: {json}");
    }
    let input_kind = arr[1].as_array().and_then(|a| a.first()).and_then(Value::as_str);
    match input_kind {
        // The zoom level is NOT resolved here: this compiler runs once for
        // the whole style, not once per zoom bucket, so every bracket is
        // carried and `TextAnchorValue::resolve` picks one at bake time --
        // the same way `LayoutValue::ZoomOfDataStep` already carries
        // `text-size`'s brackets.
        Some("zoom") => {
            let mut brackets = vec![(0u8, bracket(&arr[2], json)?)];
            let mut i = 3;
            while i + 1 < arr.len() {
                let zoom = arr[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("text-anchor zoom stops must be numeric: {json}"))?
                    as u8;
                brackets.push((zoom, bracket(&arr[i + 1], json)?));
                i += 2;
            }
            Ok(TextAnchorValue::ZoomOfDataStep(brackets))
        }
        Some("get") => Ok(TextAnchorValue::ZoomOfDataStep(vec![(0, bracket(json, json)?)])),
        _ => bail!("unsupported \"step\" input for text-anchor: {json}"),
    }
}

pub(crate) fn compile_layout_value_pair(
    json: &Value,
) -> Result<(crate::style::LayoutValue, crate::style::LayoutValue)> {
    Ok((compile_layout_value_component(json, 0)?, compile_layout_value_component(json, 1)?))
}

/// Shared walker behind `compile_layout_value_pair`: identical
/// structure-walking control flow to `compile_layout_value` (constant /
/// zoom-step / data-step / zoom-of-data-step detection), except every
/// leaf/base value is expected to be `["literal", [x, y]]` rather than a
/// bare number, and `component` (`0` for x, `1` for y) picks which half of
/// each pair this call resolves into a scalar `LayoutValue`.
fn compile_layout_value_component(json: &Value, component: usize) -> Result<crate::style::LayoutValue> {
    use crate::style::{DataRampShape, DataStepShape, LayoutValue};

    /// Reads one axis out of a `["literal", [x, y]]` leaf.
    fn literal_leaf(json: &Value, component: usize) -> Result<f32> {
        let arr = json
            .as_array()
            .filter(|a| a.first().and_then(Value::as_str) == Some("literal"))
            .ok_or_else(|| anyhow::anyhow!("text-offset leaf must be [\"literal\", [x, y]]: {json}"))?;
        let xy = arr
            .get(1)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("text-offset literal must wrap a 2-element array: {json}"))?;
        xy.get(component)
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow::anyhow!("text-offset literal pair must be numeric: {json}"))
            .map(|n| n as f32)
    }

    /// `["step", ["get", key], base, t1, v1, ...]` where `base`/`v1`/... are
    /// each `["literal", [x, y]]` leaves.
    fn parse_data_step(arr: &[Value], json: &Value, component: usize) -> Result<DataStepShape> {
        let get_expr = arr
            .get(1)
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as step input: {json}"))?;
        if get_expr.first().and_then(|v| v.as_str()) != Some("get") {
            bail!("expected [\"get\", key] as step input: {json}");
        }
        let key = get_expr
            .get(1)
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("\"get\" needs a string key: {json}"))?
            .to_string();
        if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
            bail!("malformed data-step shape: {json}");
        }
        let base = literal_leaf(&arr[2], component)?;
        let mut stops = Vec::new();
        let mut i = 3;
        while i + 1 < arr.len() {
            let threshold = arr[i]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("data-step thresholds must be numeric: {json}"))?;
            let value = literal_leaf(&arr[i + 1], component)?;
            stops.push((threshold, value));
            i += 2;
        }
        Ok(DataStepShape { key, base, stops })
    }

    /// `["interpolate", ["linear"], ["get", key], v0, o0, v1, o1, ...]`
    /// where every stop OUTPUT is a `["literal", [x, y]]` leaf -- a value
    /// that ramps with a feature property rather than with zoom.
    ///
    /// `poi-label`'s real `text-offset` is two of these, one per zoom
    /// bracket. Only `["linear"]` is accepted; see `DataRampShape`'s doc
    /// comment for why a curve is refused rather than approximated.
    fn parse_data_ramp(arr: &[Value], json: &Value, component: usize) -> Result<DataRampShape> {
        if arr.first().and_then(Value::as_str) != Some("interpolate") {
            bail!("expected an \"interpolate\" over a feature property: {json}");
        }
        if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
            bail!("\"interpolate\" must have the form [\"interpolate\", <type>, <input>, s1, v1, ...]: {json}");
        }
        if arr[1].as_array().and_then(|t| t.first()).and_then(Value::as_str) != Some("linear") {
            bail!(
                "a property-driven \"interpolate\" is supported for [\"linear\"] only, not {}: {json}",
                arr[1]
            );
        }
        let get = arr[2]
            .as_array()
            .filter(|g| g.first().and_then(Value::as_str) == Some("get"))
            .ok_or_else(|| anyhow::anyhow!("expected [\"get\", key] as interpolate input: {json}"))?;
        let key = get
            .get(1)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("\"get\" needs a string key: {json}"))?
            .to_string();
        let mut stops = Vec::new();
        let mut i = 3;
        while i + 1 < arr.len() {
            let input = arr[i]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("interpolate stop inputs must be numeric: {json}"))?;
            stops.push((input, literal_leaf(&arr[i + 1], component)?));
            i += 2;
        }
        stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        if stops.is_empty() {
            bail!("a property-driven \"interpolate\" needs at least one stop: {json}");
        }
        Ok(DataRampShape { key, stops })
    }

    /// Whether an expression array is an `interpolate` whose INPUT is a
    /// feature property -- the shape `compile_zoom_interpolate_layout_value`
    /// cannot take (it compiles zoom curves) and `parse_data_ramp` can.
    fn is_property_interpolate(arr: &[Value]) -> bool {
        arr.first().and_then(Value::as_str) == Some("interpolate")
            && arr.get(2).and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str) == Some("get")
    }

    // Top-level constant: a single ["literal", [x, y]] pair, no step.
    if json.as_array().and_then(|a| a.first()).and_then(Value::as_str) == Some("literal") {
        return Ok(LayoutValue::Constant(literal_leaf(json, component)?));
    }
    let arr = json
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("layout value-pair must be [\"literal\",[x,y]] or an expression array: {json}"))?;
    // An `interpolate` reaches the SAME compiler the scalar walker uses,
    // by rewriting this walker's `["literal", [x, y]]` stop outputs down to
    // the bare number for `component` first and handing the result to
    // `compile_zoom_interpolate_layout_value`.
    //
    // That rewrite is the whole of the work, and doing it this way rather
    // than resampling the curve here is deliberate: both real interpolation
    // types, the resampling at every integer zoom (which is what lets a
    // fractional stop zoom survive `ZoomValue`'s `u8` keys), and the refusal
    // of a PROPERTY-driven (`["get", ...]`-input) curve all live in that one
    // compiler already, and duplicating any of them is exactly how the two
    // walkers would come to disagree about what a curve means.
    //
    // `transit-stop-label` -- Mapbox Standard's bus stops and MRT stations
    // -- is the real input: `["interpolate", ["linear"], ["zoom"], 0,
    // ["literal", [0, 0]], 17, ["literal", [0, 0.75]]]`, i.e. the label
    // slides below its icon as the map zooms in. A property-driven pair
    // interpolate (`poi-label`'s, over `sizerank`) is still an `Err`, which
    // is correct: `LayoutValue` has no per-feature RAMP, only `DataStep`'s
    // `>=` buckets, and the caller degrades the property instead.
    if arr.first().and_then(|v| v.as_str()) == Some("interpolate") {
        // A curve over a feature PROPERTY, not over zoom: one bracket at
        // zoom 0, since `LayoutValue` has no unwrapped ramp variant.
        if is_property_interpolate(arr) {
            return Ok(LayoutValue::ZoomOfDataRamp(vec![(0, parse_data_ramp(arr, json, component)?)]));
        }
        if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
            bail!("\"interpolate\" must have the form [\"interpolate\", <type>, <input>, s1, v1, ...]: {json}");
        }
        let mut scalar = vec![arr[0].clone(), arr[1].clone(), arr[2].clone()];
        let mut i = 3;
        while i + 1 < arr.len() {
            scalar.push(arr[i].clone());
            scalar.push(Value::from(literal_leaf(&arr[i + 1], component)?));
            i += 2;
        }
        return compile_zoom_interpolate_layout_value(&Value::Array(scalar));
    }
    if arr.first().and_then(|v| v.as_str()) != Some("step") {
        bail!("unsupported layout value-pair expression shape: {json}");
    }
    if arr.len() < 5 || !(arr.len() - 3).is_multiple_of(2) {
        bail!("\"step\" expression must have the form [\"step\", input, base, t1, v1, ...]: {json}");
    }
    let input = arr
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("\"step\" needs an input expression: {json}"))?;
    let input_kind = input.as_array().and_then(|a| a.first()).and_then(|v| v.as_str());
    match input_kind {
        Some("zoom") => {
            let base_json = &arr[2];
            // Composite shape #2: each zoom bracket is an `interpolate` over
            // a feature property. This is `poi-label`'s real `text-offset`,
            // the expression that pushes a POI name clear of its own icon,
            // and until it compiled the label was drawn straight on the
            // sprite. Checked BEFORE the data-step case below because the
            // two are mutually exclusive and this one is the newer arrival.
            if base_json.as_array().is_some_and(|a| is_property_interpolate(a)) {
                let mut brackets = vec![(0u8, parse_data_ramp(base_json.as_array().unwrap(), json, component)?)];
                let mut i = 3;
                while i + 1 < arr.len() {
                    let zoom = arr[i]
                        .as_f64()
                        .ok_or_else(|| anyhow::anyhow!("step zoom stops must be numeric: {json}"))?
                        as u8;
                    let inner = arr[i + 1]
                        .as_array()
                        .ok_or_else(|| anyhow::anyhow!("expected a nested property interpolate at each zoom stop: {json}"))?;
                    brackets.push((zoom, parse_data_ramp(inner, json, component)?));
                    i += 2;
                }
                return Ok(LayoutValue::ZoomOfDataRamp(brackets));
            }
            // Composite shape: the base itself is a nested ["step", ["get",
            // key], ...] -- same detection as `compile_layout_value`.
            if let Some(base_arr) = base_json.as_array() {
                if base_arr.first().and_then(|v| v.as_str()) == Some("step")
                    && base_arr.get(1).and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str())
                        == Some("get")
                {
                    let mut zoom_stops = vec![(0u8, parse_data_step(base_arr, json, component)?)];
                    let mut i = 3;
                    while i + 1 < arr.len() {
                        let zoom = arr[i]
                            .as_f64()
                            .ok_or_else(|| anyhow::anyhow!("step zoom stops must be numeric: {json}"))?
                            as u8;
                        let inner = arr[i + 1]
                            .as_array()
                            .ok_or_else(|| anyhow::anyhow!("expected a nested data-step at each zoom stop: {json}"))?;
                        zoom_stops.push((zoom, parse_data_step(inner, json, component)?));
                        i += 2;
                    }
                    return Ok(LayoutValue::ZoomOfDataStep(zoom_stops));
                }
            }
            // Plain zoom-only step, leaves are literal pairs.
            let base = literal_leaf(base_json, component)?;
            let mut stops = vec![(0u8, base)];
            let mut i = 3;
            while i + 1 < arr.len() {
                let zoom = arr[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("step zoom stops must be numeric: {json}"))? as u8;
                let value = literal_leaf(&arr[i + 1], component)?;
                stops.push((zoom, value));
                i += 2;
            }
            Ok(LayoutValue::ZoomStep(ZoomValue::stepped(stops)))
        }
        Some("get") => Ok(LayoutValue::DataStep(parse_data_step(arr, json, component)?)),
        _ => bail!("unsupported \"step\" input for layout value-pair: {json}"),
    }
}

/// Compiles a `"symbol"` layer's `layout`/`paint` into one or more
/// `StyleLayer::Symbol`s. `"symbol-placement"` accepts either shape: a
/// constant `"point"` (the default) or `"line"` (e.g. one-way road arrows),
/// or a zoom-stepped `["step", ["zoom"], "point", 11, "line"]` (e.g.
/// `road-number-shield`, point below its step and line above it) --
/// `parse_placement_keyword` is the one place either shape's keywords are
/// validated, so an unrecognised keyword is rejected the same way in
/// both. That keyword
/// rejection, and a layer with neither a `text-field` nor an `icon-image`
/// (or with a `text-field` shape `compile_text_field` doesn't recognize),
/// are treated as any other in-scope-but-unsupported construct: an `Err`,
/// which the caller (`compile_layer`'s `"symbol"` dispatch arm) turns into a
/// loud per-layer skip rather than a whole-style failure.
///
/// An `icon-image` shape `compile_icon_image` doesn't recognize is the one
/// asymmetric case: it does NOT sink a layer whose `text-field` compiles.
/// Such a layer compiles as text-only, with the icon dropped and a loud
/// stderr notice -- see the comment at the `icon_image` binding below.
///
/// `paint`-side `text-color` can be a `match` expression varying by feature
/// property (the real `poi-label` layer's `text-color` varies by POI
/// `class`), so it's compiled through `compile_zoom_value`'s cross-product
/// expansion -- the same machinery `compile_layer`'s `"fill"`/`"line"` arms
/// use for `fill-color`/`line-color` -- yielding one `StyleLayer::Symbol` per
/// `text-color` case, each with that case's filter ANDed into the layer's own
/// top-level filter via `and_filters`. `text-halo-color` is compiled the same
/// way, and the two expand as a cross-product; every other field
/// (`text_field`, `icon_image`, `text_size`, `text_offset_x`/`_y`,
/// `text_anchor`, `text_max_width`, `icon_size`, `text_halo_width`,
/// `placement`, `spacing_px`,
/// `icon_rotate_deg`, `rotation_alignment`, `text_rotation_alignment`,
/// `bypass_collision`, `min_zoom`,
/// `max_zoom`) is identical across all expanded cases. A non-`match` (or
/// absent) color produces exactly one case, same as today.
///
/// Called both from `compile_layer`'s dispatch AND directly by this
/// module's own unit tests, so it re-derives everything (`id`,
/// `source-layer`, `minzoom`/`maxzoom` via the same `zoom_bound` helper
/// `compile_layer` already uses for every layer type) from the full layer
/// JSON rather than accepting already-parsed pieces as arguments.
/// Compiles a `"symbol-sort-key"` layout property, the within-layer
/// collision-placement priority (LOWER places first -- Mapbox's semantics,
/// which `crate::collision::place_symbols` implements directly).
///
/// Returns `Option`, not `Result`, and that is the whole point: a shape this
/// doesn't understand is reported as "no sort key", never as an error. A
/// sort key is a TIEBREAK, so losing it costs a layer only its within-layer
/// ordering, whereas an `Err` here would soft-skip the entire layer (see
/// `compile_layer`'s `"symbol"` arm) and cost it every one of its labels.
/// That trade is the same one `compile_symbol_layer` already makes for an
/// unsupported `icon-image`, one step further: not even a warning, because
/// unlike a dropped icon nothing visible changes.
///
/// Scoped to the two shapes that matter: a bare literal number, and
/// `["get", key]`. The latter is what both vendored users of the property
/// declare -- `settlement-major-label` and `settlement-minor-label` both use
/// `["get", "symbolrank"]`, and no other layer in either vendored style sets
/// the property at all.
fn compile_symbol_sort_key(json: &Value) -> Option<crate::style::SymbolSortKey> {
    use crate::style::SymbolSortKey;
    if let Some(n) = json.as_f64() {
        return Some(SymbolSortKey::Literal(n as f32));
    }
    let arr = json.as_array()?;
    match (arr.first().and_then(Value::as_str), arr.len()) {
        (Some("get"), 2) => Some(SymbolSortKey::Get(arr[1].as_str()?.to_string())),
        _ => None,
    }
}

pub(crate) fn compile_symbol_layer(layer_json: &Value, scope: Scope<'_>) -> Result<Vec<StyleLayer>> {
    use crate::style::LayoutValue;

    let id = layer_json["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("layer missing string \"id\": {layer_json}"))?
        .to_string();
    let source_layer = layer_json["source-layer"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("symbol layer {id:?} missing \"source-layer\""))?
        .to_string();
    let layout = &layer_json["layout"];

    let placement = match layout.get("symbol-placement") {
        None => SymbolPlacementValue::Constant(SymbolPlacement::Point),
        Some(Value::String(s)) => SymbolPlacementValue::Constant(parse_placement_keyword(s, &id)?),
        Some(Value::Array(step)) if step.first().and_then(Value::as_str) == Some("step") => {
            if step.len() < 5
                || step[1].as_array().and_then(|i| i.first()).and_then(Value::as_str) != Some("zoom")
                || step.len() % 2 != 1
            {
                bail!("symbol layer {id:?} has an unsupported stepped symbol-placement: {:?}", step);
            }
            let base = step[2]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("symbol layer {id:?} stepped symbol-placement needs a string base"))?;
            let mut stops = vec![(0u8, parse_placement_keyword(base, &id)?)];
            let mut i = 3;
            while i + 1 < step.len() {
                let stop_zoom = step[i]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("symbol layer {id:?} stepped symbol-placement needs numeric stops"))?
                    as u8;
                let keyword = step[i + 1]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("symbol layer {id:?} stepped symbol-placement needs string outputs"))?;
                stops.push((stop_zoom, parse_placement_keyword(keyword, &id)?));
                i += 2;
            }
            stops.sort_by_key(|(z, _)| *z);
            SymbolPlacementValue::ZoomStep(stops)
        }
        Some(other) => bail!(
            "symbol layer {id:?} uses unsupported symbol-placement {other} \
             (constant or zoom-stepped \"point\"/\"line\" is in scope)"
        ),
    };

    let filter = match layer_json.get("filter") {
        Some(f) => compile_filter(f, scope)?,
        None => Filter::Any,
    };

    let text_field = match layout.get("text-field") {
        Some(v) => Some(compile_text_field(v)?),
        None => None,
    };
    // An `icon-image` shape `compile_icon_image` doesn't understand is NOT
    // fatal to the layer as long as its text compiles. Failing the whole
    // layer soft-skips it (see `compile_layer`'s `"symbol"` arm), which for
    // a label layer throws away the text too -- and the text is the point of
    // a label layer, the icon is decoration. No vendored layer currently
    // reaches this path: `road-number-shield`'s `case`/`coalesce`-of-`image`-
    // of-`concat` shape used to be the standing example, but it's now
    // in scope for `compile_icon_image` (see that function's doc comment,
    // and `IconImage::CaseHas`'s), so it takes the `Ok` branch below and
    // never falls into this degradation. This branch stays for the general
    // case: some future `icon-image` shape that fails to compile but whose
    // layer still carries a usable `text-field` should degrade to text-only
    // rather than lose its labels too. So: drop the icon, keep the layer,
    // and say so loudly on stderr in the module's established
    // `style_spec: ...` format.
    // With no usable text-field, though, a layer whose icon we've just
    // dropped would draw literally nothing -- keep skipping that outright
    // rather than emit an empty layer that costs per-frame work for no
    // pixels.
    let icon_image = match layout.get("icon-image") {
        Some(v) => match compile_icon_image(v) {
            Ok(icon) => Some(icon),
            Err(error) if text_field.is_some() => {
                eprintln!(
                    "style_spec: layer {id:?}: dropping unsupported \"icon-image\", compiling as text-only: {error:#}"
                );
                None
            }
            Err(error) => return Err(error),
        },
        None => None,
    };
    if text_field.is_none() && icon_image.is_none() {
        bail!("symbol layer {id:?} has neither text-field nor icon-image");
    }
    // Line placement needs neither an icon nor a particular
    // `text-rotation-alignment` to be worth compiling: `match_tile_symbols`'s
    // `Line` arm resolves BOTH `icon_image` and `text_field` and keeps the
    // feature when either one lands. A text-only line-placed layer -- the
    // real vendored `waterway-label`, `ferry-aerialway-label`, and every
    // OpenMapTiles `highway-name-*` layer -- therefore draws, so nothing is
    // refused here on placement grounds and there is no permanently-empty
    // `StyleLayer` to warn about.

    let text_size = match layout.get("text-size") {
        Some(v) => compile_layout_value(v)?.with_fallback(16.0),
        None => LayoutValue::Constant(16.0), // real Mapbox style-spec default
    };
    let text_font = compile_text_font(layout.get("text-font"), &id, scope);
    // A `text-offset` this crate cannot read costs the layer its OFFSET,
    // not its symbols. Exactly the trade `text-anchor` makes a few lines
    // below (and `line-dasharray` makes on the paint side): a label a little
    // out of place is a far smaller defect than a layer with no labels and
    // no icons at all, which is what an `Err` here would mean --
    // `compile_layer`'s `"symbol"` arm soft-skips the whole layer.
    // `poi-label` is the real case: its `text-offset` is a zoom step whose
    // brackets are `["interpolate", ["linear"], ["get", "sizerank"], ...]`
    // ramps, and `LayoutValue` has no per-feature ramp to hold one. That
    // layer's `text-anchor` is data-driven too and is ALREADY degraded to
    // `center` below, so its label sits on its icon either way; the icon is
    // what the layer is for.
    let degrade_text_offset = |what: &Value, why: &str| -> (LayoutValue, LayoutValue) {
        let notice = format!(
            "layer {id:?}: text-offset {what} is not a shape this crate can read ({why}); \
             falling back to no offset and keeping the layer"
        );
        eprintln!("style_spec: {notice}");
        record_degradation(notice);
        (LayoutValue::Constant(0.0), LayoutValue::Constant(0.0))
    };
    let (text_offset_x, text_offset_y) = match layout.get("text-offset") {
        Some(Value::Array(pair)) if pair.len() == 2 && pair[0].as_str() != Some("literal") => {
            (compile_layout_value(&pair[0])?, compile_layout_value(&pair[1])?)
        }
        Some(v) if v.get(0).and_then(Value::as_str) == Some("literal") => {
            let xy = v[1]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("text-offset literal must wrap a 2-element array: {v}"))?;
            (
                LayoutValue::Constant(
                    xy.first()
                        .and_then(Value::as_f64)
                        .ok_or_else(|| anyhow::anyhow!("text-offset x must be numeric: {v}"))? as f32,
                ),
                LayoutValue::Constant(
                    xy.get(1)
                        .and_then(Value::as_f64)
                        .ok_or_else(|| anyhow::anyhow!("text-offset y must be numeric: {v}"))? as f32,
                ),
            )
        }
        None => (LayoutValue::Constant(0.0), LayoutValue::Constant(0.0)),
        // The composite zoom-of-data-step shape, and the zoom `interpolate`
        // (`transit-stop-label`'s real text-offset), both with an
        // `["literal", [x, y]]` pair at every leaf instead of a bare number
        // -- see `compile_layout_value_pair`. Either may still come back an
        // `Err` (a property-driven interpolate does), which the arm below
        // degrades rather than skipping the layer over.
        Some(v) if matches!(v.get(0).and_then(Value::as_str), Some("step" | "interpolate")) => {
            match compile_layout_value_pair(v) {
                Ok(pair) => pair,
                Err(error) => degrade_text_offset(v, &format!("{error:#}")),
            }
        }
        Some(other) => degrade_text_offset(other, "unsupported shape"),
    };
    // `layout.text-radial-offset`, in ems: Mapbox Streets' settlement
    // labels below z8 (`["step", ["zoom"], ["match", ["get", "capital"], 2,
    // 0.6, 0.55], 8, 0]`), which is what holds a city's name clear of its
    // town dot on the side `text-anchor` names. A shape this crate cannot
    // read degrades to none, keeping the layer -- the `text-offset` trade.
    let text_radial_offset = match layout.get("text-radial-offset") {
        None => None,
        Some(v) => match compile_layout_value(v).or_else(|_| compile_zoom_of_numeric_match(v)) {
            Ok(value) => Some(value),
            Err(error) => {
                let notice = format!(
                    "layer {id:?}: dropping unsupported \"text-radial-offset\" {v}, \
                     falling back to no radial offset: {error:#}"
                );
                eprintln!("style_spec: {notice}");
                record_degradation(notice);
                None
            }
        },
    };
    // `layout.text-anchor`: which part of the label lands on the anchor
    // point `text-offset` just moved. Read as the style spec's KEYWORD and
    // nothing else -- no expression evaluation -- so the two shapes that
    // don't yield one (an unknown string, and the expression form Mapbox
    // Streets uses on five layers, e.g. `transit-label`'s
    // `["match", ["get", "stop_type"], "entrance", "left", "top"]`) both
    // degrade to the spec default of `center`, keeping the layer. That is
    // the `line-dasharray` trade again: an anchor this crate can't read is
    // a label in the wrong place, while an Err here would cost the layer
    // every symbol it has.
    let text_anchor = match layout.get("text-anchor") {
        None => crate::style::TextAnchorValue::default(),
        Some(v) => compile_text_anchor(v).unwrap_or_else(|error| {
            let notice = format!(
                "layer {id:?}: dropping unsupported \"text-anchor\" {v}, \
                 defaulting to the style-spec anchor of \"center\": {error:#}"
            );
            eprintln!("style_spec: {notice}");
            record_degradation(notice);
            crate::style::TextAnchorValue::default()
        }),
    };

    // `layout.text-max-width`: the width, in EMS, past which the label
    // wraps. Read as a literal number and nothing else -- no expression
    // evaluation -- so the shapes that don't yield one (the `match` form
    // Mapbox Streets uses on `water-point-label` and `transit-label`, a
    // string, and the non-positive numbers the spec's own `minimum: 0`
    // rules out) all degrade to the spec default of 10 em with the layer
    // intact. Same trade as `text-anchor` above: a label that wraps at the
    // wrong width costs less than a layer with no symbols at all.
    //
    // Note that ABSENT is not "no wrapping" -- it is 10 em. See
    // `DEFAULT_TEXT_MAX_WIDTH_EM`.
    let text_max_width = match layout.get("text-max-width") {
        None => DEFAULT_TEXT_MAX_WIDTH_EM,
        Some(v) => match v.as_f64() {
            Some(ems) if ems.is_finite() && ems > 0.0 => ems as f32,
            _ => {
                eprintln!(
                    "style_spec: layer {id:?}: dropping unsupported \"text-max-width\" {v}, \
                     defaulting to the style-spec width of {DEFAULT_TEXT_MAX_WIDTH_EM} em"
                );
                DEFAULT_TEXT_MAX_WIDTH_EM
            }
        },
    };

    // A multiplier on the sprite's own pixel size; `1` (draw at native size)
    // is the real Mapbox style-spec default. Compiled through the same
    // `compile_layout_value` path as `text-size` -- which now includes zoom
    // `interpolate` curves, the shape `crosswalks`' real `icon-size` uses --
    // so the shapes it still doesn't support (arithmetic stop outputs,
    // string-keyed `match` stop outputs) make the whole layer soft-skip
    // rather than silently rendering at the wrong scale.
    let icon_size = match layout.get("icon-size") {
        Some(v) => compile_layout_value(v)?,
        None => LayoutValue::Constant(1.0),
    };
    let symbol_sort_key = layout.get("symbol-sort-key").and_then(compile_symbol_sort_key);

    // Every property below degrades to its style-spec default rather than
    // erroring: losing an arrow's 180-degree flip is a cosmetic regression,
    // while an Err here would cost the layer every one of its symbols. Same
    // trade as `compile_symbol_sort_key` and the `"line"` arm's dasharray.
    // Style-spec default 250. Degrades to that default rather than erroring,
    // like every other layout property here.
    let spacing_px = match layout.get("symbol-spacing") {
        Some(v) => compile_layout_value(v).unwrap_or_else(|error| {
            eprintln!(
                "style_spec: layer {id:?}: dropping unsupported \"symbol-spacing\" {v}, \
                 defaulting to the style-spec spacing of 250px: {error:#}"
            );
            LayoutValue::Constant(250.0)
        }),
        None => LayoutValue::Constant(250.0),
    };
    let icon_rotate_deg = layout.get("icon-rotate").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    // The spec's `auto` (and absence, and anything unrecognised): map-aligned
    // under line placement, viewport-aligned under point. A zoom-stepped
    // placement resolves this from its base (zoom-0) entry; no vendored
    // layer actually reaches this arm while stepped (`road-number-shield`
    // sets both `icon-rotation-alignment` and `text-rotation-alignment`
    // explicitly).
    let auto_rotation_alignment = || match placement.at(0) {
        SymbolPlacement::Line | SymbolPlacement::LineCenter => RotationAlignment::Map,
        SymbolPlacement::Point => RotationAlignment::Viewport,
    };
    let rotation_alignment = match layout.get("icon-rotation-alignment").and_then(Value::as_str) {
        Some("map") => RotationAlignment::Map,
        Some("viewport") => RotationAlignment::Viewport,
        _ => auto_rotation_alignment(),
    };
    // `layout.text-rotation-alignment`: same keyword, same `auto` resolution,
    // compiled into its OWN field (`text_rotation_alignment`) since a style
    // can set it independently of `icon-rotation-alignment` -- and
    // `road-number-shield` sets both, to the same value, but that is a
    // property of the one real layer, not a rule this compiler enforces.
    let text_rotation_alignment = match layout.get("text-rotation-alignment").and_then(Value::as_str) {
        Some("map") => RotationAlignment::Map,
        Some("viewport") => RotationAlignment::Viewport,
        _ => auto_rotation_alignment(),
    };
    // There used to be a compile-time notice here, warning that a
    // line-placed layer with a `Map`-aligned `text-rotation-alignment` would
    // have its text dropped on the bake path. `match_tile_symbols` no longer
    // drops it, so the notice would now be a warning about something that
    // does not happen -- worse than no notice at all.
    // `layout.icon-text-fit` / `icon-text-fit-padding`, parsed as the
    // overlay paint parses them. A value this compiler cannot read (an
    // expression) degrades to Mapbox's default, `none`: the icon at its
    // authored size, which is what every layer drew before this was read.
    let icon_text_fit = layout.as_object().map_or_else(crate::style::IconTextFit::default, |properties| {
        crate::style::IconTextFit {
            fit: crate::overlay::paint::icon_text_fit(properties).ok().flatten().unwrap_or_default(),
            padding: crate::overlay::paint::padding(properties, "icon-text-fit-padding").ok().flatten().unwrap_or([0.0; 4]),
        }
    });
    let bypass_collision = layout.get("icon-allow-overlap").and_then(Value::as_bool).unwrap_or(false)
        && layout.get("icon-ignore-placement").and_then(Value::as_bool).unwrap_or(false);

    let paint = |key: &str| layer_json.get("paint").and_then(|p| p.get(key));
    // `text-color`, like `fill-color`/`line-color`, can be a `match`
    // expression varying by feature property -- the real `poi-label` layer's
    // `text-color` varies by POI `class` (food_and_drink vs. park_like vs.
    // ...). Routed through the same `compile_zoom_value` cross-product
    // machinery `fill`/`line` paint colors already use (see `compile_layer`'s
    // `"fill"` arm), rather than a single direct `color_leaf` call that only
    // accepts a plain CSS color string, so one `(Filter, ZoomValue<Rgba>)`
    // pair per `match` case comes back (or a single `(Filter::Any, ...)` pair
    // for a plain color/absent property, same as fill/line).
    let text_colors = match paint("text-color") {
        Some(v) => or_degrade(
            compile_zoom_value(v, color_leaf, scope),
            &id,
            "\"text-color\"",
            Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        )?,
        None => vec![(Filter::Any, ZoomValue::constant(Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }))],
    };
    // `text-halo-color` gets EXACTLY the same treatment as `text-color`
    // above, for exactly the same reason: it too is expression-shaped in the
    // real vendored style. Four symbol layers there have one -- `road-label`
    // and `ferry-aerialway-label` a `match` on `class`, `continent-label` and
    // `country-label` an `["interpolate", ["linear"], ["zoom"], ...]` alpha
    // ramp between two whites -- and a bare `color_leaf` call, which only
    // accepts a plain CSS color string, hard-failed every one of them (and
    // with it the whole layer). Routing it through `compile_zoom_value` too
    // means the two sibling color properties share one convention rather
    // than the halo quietly having a narrower one.
    let text_halo_colors = match paint("text-halo-color") {
        Some(v) => or_degrade(
            compile_zoom_value(v, color_leaf, scope),
            &id,
            "\"text-halo-color\"",
            Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.0 },
        )?,
        None => vec![(Filter::Any, ZoomValue::constant(Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.0 }))],
    };
    let text_halo_width = match paint("text-halo-width") {
        Some(v) => compile_layout_value(v)?,
        None => LayoutValue::Constant(0.0),
    };
    // `text-halo-blur`, compiled through the very same call, because it is
    // the very same kind of property: a paint number the style may drive
    // from zoom or from a feature tag. The style-spec default is `0`, a hard
    // edge, which is what every layer that does not set one gets.
    let text_halo_blur = match paint("text-halo-blur") {
        Some(v) => compile_layout_value(v)?,
        None => LayoutValue::Constant(0.0),
    };

    // The icon's own three, which apply ONLY to an SDF sprite (the style
    // spec's rule: a plain sprite carries its own colour and ignores them).
    //
    // Deliberately NOT joined to the `text-color` x `text-halo-color`
    // cross-product below. That machinery expands a property-varying colour
    // into one whole `StyleLayer` per `match` case, and adding two more
    // dimensions to it would multiply the layer count for a pair of
    // properties no vendored basemap layer varies per feature -- Mapbox
    // Streets uses `icon-color` on shields, always as a flat value.
    // `single_zoom_case` therefore accepts constants and zoom expressions
    // and rejects a data-driven one by NAME, so a style that does something
    // this cannot represent is skipped with a message rather than rendered
    // wrong.
    //
    // Data-driven icon colour is exactly what a caller's own overlay wants
    // ("this route's colour"), and that path does not come through here at
    // all: `overlay::paint::FeaturePaint` reads it per feature.
    let icon_color = single_zoom_case(
        paint("icon-color"),
        &id,
        "icon-color",
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 },
        scope,
    )?;
    let icon_halo_color = single_zoom_case(
        paint("icon-halo-color"),
        &id,
        "icon-halo-color",
        Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 },
        scope,
    )?;
    let icon_halo_width = match paint("icon-halo-width") {
        Some(v) => compile_layout_value(v)?,
        None => LayoutValue::Constant(0.0),
    };
    // `icon-opacity` and `text-opacity`, compiled through EXACTLY the call
    // `compile_layer`'s `"fill"`/`"line"` arms already make for
    // `fill-opacity`/`line-opacity`: `compile_zoom_value` with
    // `number_leaf`. The resulting `(Filter, ZoomValue<f32>)` cases are
    // carried on the layer as a `SymbolOpacity` and resolved per feature at
    // bake time, rather than cross-producted into the layer expansion below
    // -- see `SymbolOpacity`'s doc comment for why a symbol layer resolves
    // where a fill layer multiplies.
    //
    // Not compiling these at all was a real, visible defect: every one of
    // Mapbox Standard's zoom ramps (`poi-label` 0 until z17,
    // `transit-stop-label` 0 until z16, `traffic signals` 0 until z16,
    // `road exit shield` 0 until z19) was ignored, so at z15 this renderer
    // drew roughly eighteen POI/transit labels and icons that the reference
    // draws none of -- the user-visible "more objects than Mapbox" report.
    //
    // Degraded, never skipped: `symbol_opacity` absorbs ANY compile error
    // into the style-spec default of 1, records it, and keeps the layer.
    // The `try_or_skip` at `compile_layer`'s `"symbol"` arm would otherwise
    // throw away every label and icon the layer has over one paint
    // property -- the same trade `text-anchor` and `line-dasharray` already
    // make, and the same direction: too many symbols is a worse-looking map,
    // no symbols is a missing one.
    let icon_opacity = symbol_opacity(paint("icon-opacity"), &id, "\"icon-opacity\"", scope);
    let text_opacity = symbol_opacity(paint("text-opacity"), &id, "\"text-opacity\"", scope);
    // Style-spec default `1` for both -- NOT `0`, unlike every other
    // `*-emissive-strength` property this module compiles. Cross-producted
    // into the `text_colors x text_halo_colors` product below, same as
    // `fill`/`line`'s own emissive strength: a real `DataDrivenProperty`,
    // even though no vendored `icon`/`text-emissive-strength` varies by
    // feature today (see `compile_emissive_strength`'s doc comment).
    let icon_emissive_strengths = compile_emissive_strength(paint("icon-emissive-strength"), &id, "\"icon-emissive-strength\"", 1.0, scope);
    let text_emissive_strengths = compile_emissive_strength(paint("text-emissive-strength"), &id, "\"text-emissive-strength\"", 1.0, scope);

    let min_zoom = zoom_bound(layer_json, "minzoom")?;
    let max_zoom = zoom_bound(layer_json, "maxzoom")?;

    // One `StyleLayer::Symbol` per (`text-color` case x `text-halo-color`
    // case x `icon-emissive-strength` case x `text-emissive-strength`
    // case), ANDing every case's filter into the layer's own top-level
    // filter -- exactly the `and_filters` cross-product pattern
    // `compile_layer`'s `"fill"` arm uses for `fill-color` x `fill-opacity`
    // x `fill-emissive-strength`. Every other field is identical across
    // all expanded cases. In practice this is a no-op multiplication for
    // every real layer: no vendored layer has more than one of these four
    // properties varying, so three of the four filter lists are always a
    // single `Filter::Any` case that `and_filters` drops.
    let mut compiled = Vec::new();
    for (color_filter, text_color) in &text_colors {
        for (halo_filter, text_halo_color) in &text_halo_colors {
            for (icon_emissive_filter, icon_emissive_strength) in &icon_emissive_strengths {
                for (text_emissive_filter, text_emissive_strength) in &text_emissive_strengths {
                    compiled.push(StyleLayer::Symbol {
                        id: id.clone(),
                        source_layer: source_layer.clone(),
                        filter: and_filters(vec![
                            filter.clone(),
                            color_filter.clone(),
                            halo_filter.clone(),
                            icon_emissive_filter.clone(),
                            text_emissive_filter.clone(),
                        ]),
                        text_field: text_field.clone(),
                        icon_image: icon_image.clone(),
                        text_size: text_size.clone(),
                        text_offset_x: text_offset_x.clone(),
                        text_offset_y: text_offset_y.clone(),
                        text_radial_offset: text_radial_offset.clone(),
                        text_anchor: text_anchor.clone(),
                        text_max_width,
                        icon_size: icon_size.clone(),
                        symbol_sort_key: symbol_sort_key.clone(),
                        text_color: themed_and_lit_color(text_color, text_emissive_strength, paint("text-color-use-theme"), scope),
                        text_halo_color: themed_and_lit_color(
                            text_halo_color,
                            text_emissive_strength,
                            paint("text-halo-color-use-theme"),
                            scope,
                        ),
                        text_halo_width: text_halo_width.clone(),
                        text_halo_blur: text_halo_blur.clone(),
                        text_font: text_font.clone(),
                        icon_opacity: icon_opacity.clone(),
                        text_opacity: text_opacity.clone(),
                        icon_color: themed_and_lit_color(&icon_color, icon_emissive_strength, paint("icon-color-use-theme"), scope),
                        icon_halo_color: themed_and_lit_color(
                            &icon_halo_color,
                            icon_emissive_strength,
                            paint("icon-halo-color-use-theme"),
                            scope,
                        ),
                        icon_halo_width: icon_halo_width.clone(),
                        placement: placement.clone(),
                        spacing_px: spacing_px.clone(),
                        icon_rotate_deg,
                        rotation_alignment,
                        text_rotation_alignment,
                        icon_text_fit,
                        bypass_collision,
                        min_zoom,
                        max_zoom,
                    });
                }
            }
        }
    }
    Ok(compiled)
}

/// Compiles `paint.icon-opacity`/`paint.text-opacity` -- and NEVER fails.
///
/// The shape itself goes through `compile_zoom_value` with `number_leaf`,
/// the identical call `compile_layer`'s `"fill"`/`"line"` arms make for
/// `fill-opacity`/`line-opacity`, so a `step`/`interpolate` over zoom
/// becomes one `ZoomValue` and a `match`/`case` over a feature property
/// becomes one `(Filter, ZoomValue)` pair per case for the caller to
/// cross-product.
///
/// What differs from the fill/line call is the failure handling, and it
/// differs deliberately. There, an unreadable opacity routes through
/// `try_or_skip!` and drops the layer; here dropping the layer costs every
/// label and icon it has, which for a symbol layer is the entire layer.
/// So EVERY error -- not only the constant-folder refusals `or_degrade`
/// absorbs -- falls back to the style-spec default of `1` (fully opaque,
/// i.e. exactly what this renderer did before the property was compiled at
/// all) with a recorded degradation. That is the same trade `text-anchor`,
/// `text-max-width` and `symbol-spacing` already make in this function.
fn symbol_opacity(
    value: Option<&Value>,
    layer_id: &str,
    property: &str,
    scope: Scope<'_>,
) -> crate::style::SymbolOpacity {
    use crate::style::SymbolOpacity;
    let Some(value) = value else { return SymbolOpacity::opaque() };
    match or_degrade(compile_zoom_value(value, number_leaf, scope), layer_id, property, 1.0) {
        Ok(cases) => SymbolOpacity { cases },
        Err(error) => {
            let notice = format!(
                "layer {layer_id:?}: dropping unsupported {property} {value}, \
                 defaulting to the style-spec opacity of 1: {error:#}"
            );
            eprintln!("style_spec: {notice}");
            record_degradation(notice);
            SymbolOpacity::opaque()
        }
    }
}

/// A colour property compiled as ZOOM-VARYING ONLY: constants, `step`,
/// `interpolate` -- anything `compile_zoom_value` collapses to a single
/// `Filter::Any` case.
///
/// A data-driven expression (`["match", ["get", ...], ...]`, `["get", ...]`)
/// comes back as more than one case, or fails to compile at all, and both
/// are reported as an error naming the layer and the property. That is the
/// same loud-skip contract every other unsupported shape in this module
/// has: the layer is dropped with a message on stderr, never drawn with a
/// colour nobody asked for.
fn single_zoom_case(
    value: Option<&Value>,
    layer_id: &str,
    property: &str,
    default: Rgba,
    scope: Scope<'_>,
) -> Result<ZoomValue<Rgba>> {
    let Some(value) = value else { return Ok(ZoomValue::constant(default)) };
    let mut cases = or_degrade(compile_zoom_value(value, color_leaf, scope), layer_id, property, default)?;
    if cases.len() != 1 {
        bail!(
            "symbol layer {layer_id:?} varies {property:?} per feature, which is not supported \
             for an icon paint property (only constants and zoom expressions are): {value}"
        );
    }
    Ok(cases.remove(0).1)
}

/// Compiles an `*-emissive-strength` paint property (`background`/`fill`/
/// `line`'s own, and `symbol`'s `icon`/`text` pair) into `(Filter,
/// ZoomValue<f32>)` cases -- the SAME shape `fill-color`/`line-color`/etc.
/// already produce. Only `icon-`/`text-emissive-strength` are actually
/// `"property-type": "data-driven"` in the v8 reference (`mapbox-gl-dev.js`,
/// Mapbox GL JS 3.15.0); `fill-`/`line-`/`background-emissive-strength`
/// carry no `property-type` at all and an `expression.parameters` of
/// `["zoom", "measure-light"]` only -- no `"feature"`. But the SHIPPED
/// Standard style demands the `(Filter, ZoomValue)` shape for `line`
/// anyway: the real vendored `roads`/`bridge-roads` `line-emissive-
/// strength` reads `["get", "class"]` and genuinely varies by feature,
/// spec declaration or not, once `compile_let` inlines their feature-
/// dependent `isHighClass` binding (see that function's doc comment) --
/// collapsing to one scalar here would silently throw that split away.
/// Callers cross-product this the same way they already cross-product
/// color/width/opacity/dash (see the `"fill"`/`"line"` arms of
/// `compile_layer`, and `compile_symbol_layer`'s `icon`/`text` calls) --
/// except `background`, which enforces exactly one case itself (see its
/// call site): a background layer has no features to filter by, the same
/// rule `background-color` already follows.
///
/// Defaults to `vec![(Filter::Any, ZoomValue::constant(default))]` --
/// `default` is the exact style-spec default FOR THIS PROPERTY (they are
/// not all the same; see each call site) -- both when the property is
/// absent and on ANY compile failure.
///
/// The compiled value is consumed by `light_ground_color`, which folds
/// `_prelude_lighting.glsl`'s `mix(ground_radiance, 1, strength)` into the
/// layer's colour at compile time; it is also stored on the `StyleLayer`
/// as-authored (see `StyleLayer::Fill::emissive_strength`).
///
/// Deliberately NOT routed through `try_or_skip!` the way every other
/// in-scope paint property is (see the module doc comment's taxonomy), and
/// more permissive than `or_degrade` alone: every other error `or_degrade`
/// doesn't recognize as `Unfoldable` still propagates up to `try_or_skip!`
/// and drops the whole layer, which is the right call for e.g.
/// `fill-color` -- a broken colour expression usually means a broken
/// layer. Emissive strength gets no such downstream skip: a construct this
/// function can't fold is a wrong BRIGHTNESS, never a reason to lose the
/// layer's fill/line/label altogether. So this function catches every
/// `Err`, not just `Unfoldable` ones, and always keeps the layer.
fn compile_emissive_strength(
    value: Option<&Value>,
    layer_id: &str,
    property: &str,
    default: f32,
    scope: Scope<'_>,
) -> Vec<(Filter, ZoomValue<f32>)> {
    let Some(value) = value else { return vec![(Filter::Any, ZoomValue::constant(default))] };
    match compile_zoom_value(value, emissive_strength_leaf, scope) {
        Ok(cases) => {
            // An empty `cases` would make the caller's cross-product emit
            // ZERO layers -- exactly the "lose the layer" outcome this
            // whole function exists to prevent. Not reachable today:
            // `compile_match`/the `case` dispatch arm always emit at least
            // a default/fallback pair, and every other `compile_zoom_value`
            // arm returns exactly one. Pinned rather than trusted.
            debug_assert!(!cases.is_empty(), "compile_zoom_value must never return an empty case list: {value}");
            cases
        }
        Err(error) => {
            let notice = format!(
                "layer {layer_id:?}: {property:?} could not be folded to a value, falling back to \
                 the style-spec default {default} and keeping the layer: {error:#}"
            );
            eprintln!("style_spec: {notice}");
            record_degradation(notice);
            vec![(Filter::Any, ZoomValue::constant(default))]
        }
    }
}

/// Collapses `compile_emissive_strength`'s `(Filter, ZoomValue<f32>)` cases
/// to the single, never feature-filtered value a `background` layer needs
/// -- a background layer has no features to filter by, the same rule
/// `background-color` already follows (see its own `colors.len() != 1`
/// check just above). Unlike that check, this one does NOT bail: per this
/// task's ruling, `*-emissive-strength` must never take the whole layer
/// down, so a result that genuinely comes back with more than one case
/// (not reachable by any real vendored `background` layer today) degrades
/// to `default` and keeps the layer instead.
fn single_case_emissive_strength(mut cases: Vec<(Filter, ZoomValue<f32>)>, layer_id: &str, property: &str, default: f32) -> ZoomValue<f32> {
    if cases.len() == 1 {
        return cases.remove(0).1;
    }
    let notice = format!(
        "layer {layer_id:?}: {property:?} folded into {} feature-filtered cases, which a \
         background layer (no features to filter by) cannot represent; falling back to the \
         style-spec default {default} and keeping the layer",
        cases.len()
    );
    eprintln!("style_spec: {notice}");
    record_degradation(notice);
    ZoomValue::constant(default)
}

/// `"point"`, `"line"` and `"line-center"` -- every placement the style
/// spec defines. `"line-center"` (Mapbox Streets' and Light's
/// `water-line-label`/`natural-line-label`) is one label per line part at
/// the part's midpoint; see `SymbolPlacement::LineCenter`.
fn parse_placement_keyword(keyword: &str, id: &str) -> Result<SymbolPlacement> {
    match keyword {
        "point" => Ok(SymbolPlacement::Point),
        "line" => Ok(SymbolPlacement::Line),
        "line-center" => Ok(SymbolPlacement::LineCenter),
        other => bail!("symbol layer {id:?} uses unsupported symbol-placement {other:?}"),
    }
}

/// ANDs a layer's own compiled `"filter"` together with the per-case filters
/// its paint expressions expanded into, dropping every `Filter::Any` no-op
/// so the common case (a layer filter plus a non-data-driven paint value)
/// stays a bare filter instead of a pointlessly nested `All([f, Any])`.
pub(crate) fn and_filters(filters: Vec<Filter>) -> Filter {
    let mut real: Vec<Filter> =
        filters.into_iter().filter(|f| !matches!(f, Filter::Any)).collect();
    match real.len() {
        0 => Filter::Any,
        1 => real.pop().expect("length checked to be 1"),
        _ => Filter::All(real),
    }
}

/// Reads an optional `"minzoom"`/`"maxzoom"` as `u8`.
///
/// `StyleLayer`'s zoom bounds (and `resolve_plan`'s `zoom` parameter -- see
/// its doc comment in `crate::style` for what that parameter actually is)
/// are integer-valued, so a fractional bound can't be represented exactly.
/// Liberty's 13 `minzoom`/2 `maxzoom` values are all integers, but the
/// vendored Mapbox Streets style has exactly one fractional bound anywhere
/// (checked across every layer): `continent-label`'s `"minzoom": 0.75`.
///
/// A fractional bound is resolved in the CONSERVATIVE direction -- `minzoom`
/// floors, `maxzoom` ceils -- which widens the layer's visible zoom range
/// rather than narrowing it. `0.75` floored to `0` means the continent
/// labels appear a fraction of a zoom level early; rounded to `1` they would
/// instead be MISSING for the whole of z0, at a zoom the style says they
/// should be drawn at. Showing a layer slightly too early is a far smaller
/// error than hiding it outright, and there is no third option: the
/// representation stays `u8` (`resolve_plan` compares these bounds against
/// `zoom`, an integer DISPLAY zoom -- the camera's own bucket, deliberately
/// unclamped by the tile source's ceiling, NOT `tile.z` -- see `display_zoom`'s
/// doc comment in `map_view.rs` and `resolve_plan`'s own doc comment on its
/// `zoom` parameter; a fractional display zoom is no more representable
/// here than a fractional `tile.z` would have been).
///
/// The range check itself is NOT widened: a bound outside 0..=30 is still a
/// hard error.
///
/// Note a deliberate semantic deviation from the style spec: the spec hides
/// a layer at zoom levels *greater than or equal to* `maxzoom`, whereas
/// `resolve_plan` treats `max_zoom` as inclusive. The only affected Liberty
/// layers are `landuse_residential` (maxzoom 12) and `building`
/// (minzoom 13, maxzoom 14), so the practical effect is that both stay
/// visible for one extra integer zoom -- for `building` that's harmless
/// here: the `building-3d` layer that replaces it at z14 in real MapLibre
/// is a `fill-extrusion`, which this renderer now compiles to the same
/// footprint drawn flat, so the two simply overlap for one zoom level
/// instead of one of them leaving a hole. Before the fill-extrusion
/// fallback that extra zoom was load-bearing rather than harmless -- it was
/// the only thing standing between the user and a z14 with no buildings at
/// all -- and it is worth keeping precisely because it no longer has to be.
fn zoom_bound(layer: &Value, key: &str) -> Result<Option<u8>> {
    match layer.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("{key:?} must be a number, got {value}"))?;
            if !(0.0..=30.0).contains(&number) {
                bail!("{key:?} must be a number between 0 and 30, got {value}");
            }
            // Floor a `minzoom`, ceil a `maxzoom` -- see the doc comment
            // above for why widening is the safe direction.
            let rounded = if key == "maxzoom" { number.ceil() } else { number.floor() };
            Ok(Some(rounded as u8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{DecodedTile, Feature, Layer, TagValue};
    use crate::style::{match_tile, match_tile_symbols, PaintOp, TextAnchor};
    use geo_types::{Geometry, LineString, Polygon};
    use std::collections::HashMap;

    fn polygon_geometry() -> Geometry<i32> {
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]),
            vec![],
        ))
    }

    fn line_geometry() -> Geometry<i32> {
        Geometry::LineString(LineString::from(vec![(0, 0), (10, 10)]))
    }

    fn tile_with_feature(source_layer: &str, tags: HashMap<String, TagValue>, geometry: Geometry<i32>) -> DecodedTile {
        DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: source_layer.to_string(),
                features: vec![Feature { geometry, tags: tags.into() }],
            }],
        }
    }

    fn liberty() -> Style {
        let bytes = std::fs::read("examples/liberty-style.json").expect("vendored style file");
        parse_and_compile(&bytes).expect("Liberty style must compile cleanly")
    }

    fn mapbox_streets() -> Option<Style> {
        let bytes = crate::mapbox_fixtures::read("mapbox-streets-style.json")?;
        Some(parse_and_compile(&bytes).expect("Mapbox Streets style must compile cleanly"))
    }

    /// The raw JSON of one layer of the vendored Mapbox Streets style, by
    /// `id`. Lets a test drive `compile_symbol_layer` against the REAL layer
    /// -- filter, layout and paint verbatim -- rather than a hand-copied
    /// excerpt that can drift from the file it claims to quote.
    fn real_layer(id: &str) -> Option<Value> {
        let bytes = crate::mapbox_fixtures::read("mapbox-streets-style.json")?;
        let root: Value = serde_json::from_slice(&bytes).expect("vendored style is valid JSON");
        Some(
            root["layers"]
                .as_array()
                .expect("style has a layers array")
                .iter()
                .find(|l| l["id"].as_str() == Some(id))
                .unwrap_or_else(|| panic!("vendored style has no layer {id:?}"))
                .clone(),
        )
    }

    /// One layer out of the vendored Mapbox Standard Dark document, by id,
    /// searching the ROOT document's own `layers` first and then the
    /// imported basemap's -- the two scopes may reuse an id (`poi-label`
    /// does), and the root's own layer is the one stacked on top.
    fn standard_dark_layer(id: &str) -> Option<Value> {
        let bytes = crate::mapbox_fixtures::read("mapbox-standard-dark-style.json")?;
        let root: Value = serde_json::from_slice(&bytes).expect("the vendored fixture is valid JSON");
        let imported = root["imports"][0]["data"]["layers"].clone();
        for layers in [&root["layers"], &imported] {
            if let Some(found) = layers.as_array().and_then(|a| a.iter().find(|l| l["id"].as_str() == Some(id))) {
                return Some(found.clone());
            }
        }
        panic!("the vendored Standard Dark fixture has no layer {id:?}");
    }

    fn tags(pairs: &[(&str, &str)]) -> HashMap<String, TagValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), TagValue::String(v.to_string())))
            .collect()
    }

    #[test]
    fn compiles_the_real_vendored_liberty_style_without_error() {
        let style = liberty();
        assert!(!style.layers.is_empty());

        // Spot-check a real, known layer: Liberty's `building` fill layer
        // declares "minzoom": 13, "maxzoom": 14.
        let building = style
            .layers
            .iter()
            .filter_map(|layer| match layer {
                StyleLayer::Fill { source_layer, min_zoom, max_zoom, .. }
                    if source_layer == "building" =>
                {
                    Some((*min_zoom, *max_zoom))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            building.contains(&(Some(13), Some(14))),
            "expected a building fill layer with Liberty's real z13-14 range, got {building:?}"
        );
    }

    #[test]
    fn compiles_every_in_scope_liberty_layer_and_skips_the_rest() {
        // 111 layers total in the vendored file: 84 background/fill/line
        // (1 background, 16 fill, 67 line), 25 symbol, 1 `fill-extrusion`
        // (`building-3d`) and 1 `raster` (`natural_earth`) -- and since
        // raster compiles, **no layer in this style is skipped for its
        // type at all any more**. Two of the 16 fill
        // layers are `fill-pattern`-only (`landcover_wetland`,
        // `road_area_pattern`) and are skipped too, and no Liberty paint
        // property is a feature-dependent `match`, so the compiled count is
        // 84 - 2 = 82.
        //
        // Constant `"line"` placement compiling: 7 of the 25 symbol layers
        // declare `symbol-placement: "line"` (`road_one_way_arrow`,
        // `road_one_way_arrow_opposite`, `waterway_line_label`,
        // `water_name_line_label`, `highway-name-path`, `highway-name-minor`,
        // `highway-name-major`). Of those, only the two arrow layers compile:
        // the other 5 carry TEXT, and line-placed text was at the time
        // refused outright by `compile_symbol_layer` (it needs per-glyph
        // curved placement, which glyphon's `TextArea` cannot express --
        // `symbol_pass::line_glyph_quads` does it with rotated quads
        // instead, and the refusal is long gone). That took the count to 84.
        //
        // `text-field` gaining `case`/`concat`/`to-string` (this change): 23
        // of the 25 symbol layers used to be skipped on their `text-field`
        // shape alone. 18 of them now compile -- every place name
        // (`label_country_1..3`, `label_state`, `label_city`,
        // `label_city_capital`, `label_town`, `label_village`,
        // `label_other`), every POI (`poi_r1`, `poi_r7`, `poi_r20`,
        // `poi_transit`), `airport`, `water_name_point_label`, and all three
        // road shields (`highway-shield-non-us`,
        // `highway-shield-us-interstate`, `road_shield_us`, whose
        // `["to-string", ["get", "ref"]]` is the shape that was missing).
        //
        // Line-placed TEXT compiling (this change): the 5 remaining layers
        // (`waterway_line_label`, `water_name_line_label`, and the three
        // `highway-name-*`) used to be refused at compile time. They now
        // compile, and whether they DRAW is decided in `match_tile_symbols`
        // instead -- one decision in one place. 102 + 5 = 107.
        //
        // The fill-extrusion fallback (this change): `building-3d` is the
        // one `fill-extrusion` layer here, and it now compiles to a flat
        // `StyleLayer::Fill` of its footprint instead of being skipped for
        // its type. It is the layer Liberty hands over to at z14, where
        // `building` (the flat fill, `maxzoom 14`) stops -- so before this,
        // every building in the style disappeared at exactly the zoom a
        // user goes looking for buildings. 107 + 1 = 108.
        //
        // Raster (this change): `natural_earth` is Liberty's shaded-relief
        // basemap and was the last layer this compiler turned away for its
        // type, printing `skipping layer "natural_earth": unsupported
        // layer type "raster"` on every single compile. It now compiles to
        // a `StyleLayer::Raster`. 108 + 1 = **109**, and the count of
        // layers skipped for their type is now zero.
        assert_eq!(liberty().layers.len(), 109);
        // Named, not just counted, for the same reason `building-3d` is
        // below: the raster layer must be the one that arrived, carrying
        // the source it declared and Liberty's own `maxzoom: 7`.
        let natural_earth: Vec<_> = liberty()
            .layers
            .into_iter()
            .filter(|layer| matches!(layer, StyleLayer::Raster { .. }))
            .collect();
        assert!(
            matches!(
                natural_earth.as_slice(),
                [StyleLayer::Raster { id, source, min_zoom: None, max_zoom: Some(7), .. }]
                    if id == "natural_earth" && source == "ne2_shaded"
            ),
            "expected exactly one raster layer `natural_earth` over `ne2_shaded` capped at \
             z7, got {natural_earth:?}"
        );
        // Named, not just counted: a count alone would pass if the wrong
        // layer arrived. `building-3d` compiles to exactly one `Extrusion`,
        // over the same `building` source layer as the flat `building` fill
        // it takes over from, and carries its declared `minzoom 14` with no
        // upper bound.
        let building_3d: Vec<_> = liberty().layers.into_iter().filter(|layer| layer.id() == "building-3d").collect();
        assert!(
            matches!(
                building_3d.as_slice(),
                [StyleLayer::Extrusion { source_layer, min_zoom: Some(14), max_zoom: None, .. }]
                    if source_layer == "building"
            ),
            "expected exactly one building-3d extrusion over `building` from z14, got {building_3d:?}"
        );
    }

    #[test]
    fn compiles_most_of_the_real_vendored_mapbox_streets_style() {
        // Regression test, updated in two stages:
        // 1. The comparison-form geometry-type no-op fix in
        //    filter::compile_comparison (before: only 19 of 100 in-scope
        //    layers compiled; after: 88 -- the vendored style has 134
        //    layers total, 100 of them background/fill/line (in scope),
        //    the rest symbol (32) or circle (2), out of scope).
        // 2. step/case/multi-case-group match filter support (prior fix):
        //    of the 100 in-scope layers, 92 now compile (up from 88 -- 4 more
        //    from comparison-form geometry-type layers).
        // 3. step/interpolate support for nesting paint match expressions
        //    (this plan's change): of the 100 in-scope layers, 96 now compile
        //    (up from 92 -- 4 more: road-motorway-trunk, waterway,
        //    waterway-shadow, aeroway-line, which use step/interpolate
        //    with paint-side match for line-width/line-color).
        //    Those 96 JSON layers expand into 116 StyleLayers (up from 105),
        //    with 20 of them (road-motorway-trunk, waterway, waterway-shadow,
        //    aeroway-line, and previously-compiled layers with paint-side
        //    match) producing more than one StyleLayer due to paint
        //    expansions. The remaining 4 in-scope layers stay skipped, all
        //    for documented out-of-scope reasons: landuse (filter arithmetic),
        //    water-depth (property-based fill-color interpolation),
        //    hillshade (numeric match labels), road-pedestrian-polygon-pattern
        //    (no fill-color).
        // 4. fill-opacity/line-opacity reading and cross-producting (prior
        //    fix): of the same 96 in-scope layers, the count rises to 119
        //    (up from 116). `landcover` is the only real layer whose
        //    fill-opacity is itself `match`-shaped: its fill-color already
        //    contributed 3 cases (wood/snow/default), and its new opacity
        //    cross-product multiplies those 3 color-cases by 2 opacity-cases
        //    (an explicit "crop" case vs. the default), 3 * 2 = 6
        //    StyleLayers, up from 3 -- a net +3. Every other in-scope
        //    layer's fill-opacity/line-opacity is a plain (non-match) zoom
        //    expression, so it stays a 1-element opacities list and
        //    cross-products without expanding anything, contributing no
        //    further increase.
        // 5. `to-number` key unwrap and zoom-offset arithmetic-comparison
        //    filter support (this plan's change): `landuse`'s filter is
        //    `["all", <match-based filter>, [">=", ["to-number", ["get",
        //    "sizerank"]], 0], ["<=", ["-", ["to-number", ["get",
        //    "sizerank"]], ["interpolate", ["exponential", 1.5], ["zoom"],
        //    12, 0, 18, 14]], 14]]` -- previously skipped entirely because
        //    `extract_key` didn't unwrap `to-number` and no arithmetic
        //    comparison was supported. Both are fixed now, so `landuse`
        //    joins the in-scope compiled set (97 of 100 in-scope layers now
        //    compile, up from 96), and its `fill-color`/`fill-opacity` are
        //    themselves zoom+match-nested expressions (the same machinery
        //    as `landcover`), so it alone expands into 34 StyleLayers. Total
        //    rises to 153 (up from 119, measured via a temporary print, not
        //    hand-derived, per the module's TDD requirement).
        // 6. Numeric match-label support (`Filter::NumberMultiMatch`, this
        //    change): `hillshade`'s filter is `["all", ["step", ...],
        //    ["match", ["get", "level"], 89, true, 78, [...], ..., false]]`,
        //    whose case-group labels (89, 78, 67, 56, 94, 90) are bare JSON
        //    numbers -- previously unsupported, so the whole layer was
        //    dropped. Now fixed, so `hillshade` joins the in-scope compiled
        //    set (98 of 100 in-scope layers now compile, up from 97; the
        //    remaining 2 -- `water-depth` and
        //    `road-pedestrian-polygon-pattern` -- stay skipped for their own
        //    documented out-of-scope reasons: property-based fill-color
        //    interpolation and pattern fills respectively). `hillshade`'s
        //    `fill-color` is an `interpolate` over `["zoom"]` whose stop
        //    outputs are each a 2-case `match` on `class` ("shadow" vs.
        //    default), so it expands into 2 StyleLayers. Total rises to 155
        //    (up from 153, measured via running the test, not hand-derived,
        //    per the module's TDD requirement).
        // 7. `symbol` layers enter scope (this plan's change,
        //    `compile_symbol_layer`): the vendored style has 32 `symbol`
        //    layers, all previously dropped by the generic "unsupported
        //    layer type" skip before ever being parsed. Of those 32, only 6
        //    compile with Task 2/3's scope of `text-field`/`icon-image`/
        //    layout-value expression shapes: `level-crossing`,
        //    `building-entrance`, `building-number-label`,
        //    `block-number-label`, `golf-hole-label` -- all simple
        //    point-placed icon/text layers with plain (non-`step`/
        //    `interpolate`) layout values. The other 27 stay skipped, each
        //    for a real, distinct reason surfaced only by actually running
        //    this test (not guessed): 12 use line/line-center placement
        //    (`tunnel-oneway-arrow-*`, `road-oneway-arrow-*`,
        //    `bridge-oneway-arrow-*`, `road-label`, `path-pedestrian-label`,
        //    `ferry-aerialway-label`, `waterway-label`,
        //    `natural-line-label`, `water-line-label`), out of scope per
        //    this plan's point-only Global Constraint; 13 use a `text-field`,
        //    `icon-image`, or numeric layout value shape beyond Task 2/3's
        //    scope (`road-intersection`, `road-number-shield`,
        //    `road-exit-shield`, `natural-point-label`, `water-point-label`,
        //    `transit-label`, `airport-label`,
        //    `settlement-subdivision-label`, `settlement-minor-label`,
        //    `settlement-major-label`, `state-label`, `country-label`,
        //    `continent-label`, and (since `icon-size` is now compiled at
        //    all rather than ignored) `crosswalks`, whose `icon-size` is an
        //    `interpolate` over `["zoom"]` -- `concat`-built icon/text names, nested
        //    `match`/`case` icon-image bases, and `interpolate`/
        //    `cubic-bezier` layout values, none of which Task 2/3's
        //    compilers recognize); and `poi-label`'s own `"filter"` uses
        //    arithmetic (`["+", ["step", ...], 3]`) inside a comparison,
        //    which `compile_filter` doesn't support, independent of its
        //    otherwise-in-scope layout properties. A symbol layer failing
        //    to compile for any of these reasons is a per-layer soft skip,
        //    not a whole-style failure (see `compile_layer`'s early
        //    `"symbol"` dispatch) -- notably including `continent-label`,
        //    whose real `"minzoom"` is the fractional `0.75`, a shape
        //    `zoom_bound`'s whole-number requirement can't represent;
        //    without routing symbol layers around that hard-`?` path,
        //    this ONE layer would crash the entire style's compile. Each
        //    of the 5 successfully-compiled symbol layers compiles into
        //    exactly one `StyleLayer::Symbol` (`compile_symbol_layer` never
        //    cross-product-expands like fill/line do), so total rises to
        //    160 (up from 155, measured via running the test, not
        //    hand-derived, per the module's TDD requirement).
        //
        //    `crosswalks` was in this compiled set until `icon-size` was
        //    wired up: it was compiling only because `icon-size` was being
        //    read by nothing at all, which meant its icons drew at the
        //    sprite's native size, 2-10x the 0.1-0.5 scale the layer
        //    actually asks for. Now that `icon-size` goes through
        //    `compile_layout_value` like `text-size` does, its `interpolate`
        //    expression soft-skips the layer -- the same loud, honest
        //    degrade every other unsupported layout-value shape gets, rather
        //    than silently drawing at the wrong scale. Extending
        //    `LayoutValue` to `interpolate` (follow-up scope) brings it, and
        //    several of the layers listed above, back.
        // 8. `compile_layout_value_pair` (this change): `text-offset`'s real
        //    shape is the SAME zoom-of-data-step composite `compile_layout_value`
        //    already handles for `text-size`, except every leaf/base value is
        //    an `["literal", [x, y]]` pair instead of a bare number -- e.g.
        //    poi-label's and natural-point-label's real `text-offset`
        //    expressions. Both `poi-label`'s and `natural-point-label`'s
        //    `"filter"`/`icon-image`/`text-size` were already in scope, so
        //    this was their last blocker each -- except poi-label turns out
        //    to have ANOTHER, previously-undiscovered blocker once
        //    text-offset stops masking it: its paint `text-color` is a
        //    `match` expression, which `compile_symbol_layer`'s `text-color`
        //    handling doesn't support (it calls `color_leaf` directly, which
        //    only accepts a plain CSS color string -- unlike `fill`/`line`
        //    paint colors, symbol `text-color` isn't run through
        //    `compile_zoom_value`'s match-cross-product machinery at all).
        //    That's a real, separate, undiagnosed gap, out of this change's
        //    scope (match-shaped symbol paint colors, not layout-value
        //    pairs) -- so `poi-label` stays skipped, now for that new
        //    reason instead of `text-offset`. Only `natural-point-label`
        //    newly compiles, so total rises to 161 (up from 160, measured
        //    via running the test, not hand-derived, per the module's TDD
        //    requirement).
        // 9. `text-color` match-expansion (this change): `compile_symbol_layer`
        //    now compiles `text-color` through `compile_zoom_value` (the same
        //    cross-product machinery `fill`/`line` paint colors already use)
        //    instead of a single direct `color_leaf` call, so `poi-label`'s
        //    real `text-color` -- a `match` on `class` with several
        //    single-string cases (`food_and_drink`, `park_like`, `education`,
        //    `medical`, `sport_and_leisure`), some multi-value cases
        //    (`["store_like", "food_and_drink_stores"]`, etc.), and a default
        //    -- no longer trips `color_leaf`'s "expected a CSS color string"
        //    error, clearing its last of four blockers (filter, icon-image,
        //    text-offset, now text-color). `poi-label` newly compiles into
        //    9 `StyleLayer::Symbol`s, one per `match` case group (5 single-
        //    value cases -- food_and_drink, park_like, education, medical,
        //    sport_and_leisure -- 3 multi-value case groups --
        //    [store_like, food_and_drink_stores],
        //    [commercial_services, motorist, lodging],
        //    [arts_and_entertainment, historic, landmark] -- plus 1 default),
        //    each ANDing that case's filter into poi-label's own already-
        //    working filterrank filter, so total rises to 170 (up from 161,
        //    measured via running the test with --nocapture and confirming
        //    "poi-label" no longer appears in any skip-reason line, not
        //    hand-derived, per the module's TDD requirement).
        // 10. Zoom `interpolate` layout values (this change): `compile_layout_value`
        //    now recognizes `["interpolate", <type>, ["zoom"], ...]` and
        //    delegates it to the paint side's existing
        //    `expression::compile_zoom_value` curve compiler (see
        //    `compile_zoom_interpolate_layout_value`), instead of rejecting
        //    every `interpolate` outright with "unsupported layout value
        //    expression shape". Two real symbol layers were being skipped
        //    for exactly and only that reason, and now compile:
        //    `crosswalks` (`icon-size`: `["interpolate", ["linear"],
        //    ["zoom"], 16, 0.1, 18, 0.2, 19, 0.5, 22, 1.5]` -- the layer
        //    entry 7 above explicitly predicted would come back once
        //    `LayoutValue` learned `interpolate`) and `road-intersection`
        //    (`text-size`: `["interpolate", ["exponential", 1.2], ["zoom"],
        //    15, 9, 18, 12]`). Neither has a `match`-shaped `text-color`,
        //    so each compiles into exactly one `StyleLayer::Symbol`, and
        //    the total rises to 172 (up from 170, measured by running this
        //    test, not hand-derived, per the module's TDD requirement).
        //
        //    `continent-label`'s `text-size` interpolate
        //    (`["interpolate", ["exponential", 0.5], ["zoom"], 0, 10, 2.5,
        //    15]`) also compiles now, but that layer STAYS skipped: fixing
        //    its first blocker only surfaced two more, independent ones --
        //    its paint `text-halo-color` is an `interpolate` between two
        //    color strings, which `compile_symbol_layer` reads through a
        //    bare `color_leaf` that only accepts a plain CSS color string,
        //    and its `"minzoom"` is the fractional `0.75`, which
        //    `zoom_bound` cannot represent. Both are separate documented
        //    gaps; see
        //    `continent_label_layout_value_now_compiles_but_the_layer_still_skips_for_two_other_reasons`.
        //    The remaining `interpolate`-shaped layout values in the
        //    vendored style are still out of scope for their own reasons:
        //    `settlement-subdivision-label`/`state-label`/`country-label`
        //    use a `cubic-bezier` interpolation type, and
        //    `water-point-label`'s stop outputs are arithmetic expressions
        //    (`["*", ["-", 16, ["sqrt", ["get", "sizerank"]]], 1]`) rather
        //    than numeric leaves.
        // 11. Expression-shaped `text-halo-color` + fractional zoom bounds
        //    (this change): the two blockers entry 10 left `continent-label`
        //    pinned on are both fixed. `compile_symbol_layer` now compiles
        //    `text-halo-color` through `compile_zoom_value` exactly as it
        //    already compiled `text-color` -- cross-producting the two
        //    colors' cases through `and_filters` -- instead of a bare
        //    `color_leaf` call that only accepted a plain CSS color string;
        //    and `zoom_bound` now accepts a fractional bound by flooring a
        //    `minzoom` / ceiling a `maxzoom` (the range-widening direction,
        //    see its doc comment) instead of hard-erroring. `continent-label`
        //    is the only layer either fix unblocks on its own: the other
        //    three expression-shaped `text-halo-color`s belong to
        //    `country-label` (still blocked on `cubic-bezier`, see entry 12)
        //    and to `road-label`/`ferry-aerialway-label` (both
        //    `symbol-placement: line`, out of scope), and `0.75` is the only
        //    fractional zoom bound in the whole style. It compiles into one
        //    `StyleLayer::Symbol` (its `text-halo-color` curve is zoom-only,
        //    so the cross-product is 1x1), so the total rises to 173 (up from
        //    172, measured by running this test, not hand-derived, per the
        //    module's TDD requirement).
        // 12. `cubic-bezier` + property-varying `interpolate` stops (this
        //    change): the last two shapes entries 10 and 11 listed as
        //    out-of-scope on the place labels. `expression::Interpolation`
        //    now carries all three real interpolation types (the CSS unit
        //    cubic bezier is solved parametrically, x(t) = u by
        //    Newton-Raphson with a bisection fallback, then y(t)), and
        //    `compile_interpolated_data_step_layout_value` compiles an
        //    `interpolate` whose stop OUTPUTS are `["step", ["get", key],
        //    ...]` expressions into `LayoutValue::ZoomOfDataStep` -- the
        //    variant that already meant "at each zoom, a step on a
        //    property" -- by sampling the curve at every integer zoom and
        //    mixing the bracketing stops bucket by bucket. Two real layers
        //    were blocked on exactly that pair of shapes and now compile:
        //    `state-label` (bezier [0.85,0.7,0.65,1], symbolrank buckets
        //    6/7 over z4-z9) and `country-label` (bezier [0.2,0,0.7,1],
        //    symbolrank buckets 4/5 over z1-z9, and it also needed entry
        //    11's halo-color fix). Neither has a `match`-shaped paint
        //    color, so each compiles into exactly one `StyleLayer::Symbol`
        //    and the total rises to 175 (up from 173, measured by running
        //    this test, not hand-derived, per the module's TDD
        //    requirement).
        //
        //    `settlement-subdivision-label` does NOT come back, and this is
        //    a representational limit rather than a missing feature: its
        //    `text-size` stops are `match`es on the STRING property `type`
        //    ("suburb" vs. everything else), while `DataStepShape` selects
        //    a bucket by comparing a NUMERIC tag against `>=` thresholds
        //    (`DataStepShape::resolve`). There is no faithful encoding --
        //    writing one anyway would hand every feature the default bucket
        //    while appearing to work -- so it soft-skips with an error that
        //    says exactly that; representing it would need a new
        //    per-feature `LayoutValue` shape keyed by string labels.
        //    `water-point-label` skips through the same new path, its stop
        //    outputs being arithmetic (`["*", ["-", 16, ["sqrt", ["get",
        //    "sizerank"]]], 1]`).
        // 13. Text-only degradation for an unsupported `icon-image` (this
        //    change): `compile_symbol_layer` no longer fails the WHOLE layer
        //    when `compile_icon_image` rejects its shape -- if the layer's
        //    `text-field` compiles, the layer compiles as text-only with the
        //    icon dropped and a loud stderr notice, because for a label
        //    layer the text is the point and the icon is decoration. (With
        //    no usable text-field it still skips: a layer with neither draws
        //    nothing.) Four layers stop failing on `icon-image`, but only
        //    ONE of them actually compiles, because for the other three
        //    removing the icon blocker only surfaces a second, independent
        //    one -- exactly the pattern entries 8 and 10 hit:
        //      * `road-exit-shield` (`["concat", "motorway-exit-",
        //        ["to-string", ["get", "reflen"]]]`) newly compiles, into
        //        one `StyleLayer::Symbol` (plain `text-color`), recovering
        //        its exit numbers. Total rises to 176 (up from 175, measured
        //        by running this test, not hand-derived, per the module's
        //        TDD requirement).
        //      * `road-number-shield` (the `case`/`coalesce`-of-`image`/
        //        `concat` shield-name shape) still skips, now on its paint
        //        `text-color`: a `["case", ...]` expression, which
        //        `compile_zoom_value`'s `match`-only cross-product machinery
        //        doesn't recognize ("expected a CSS color string, got
        //        [\"case\", ...]"). A separate documented gap.
        //      * `settlement-major-label` / `settlement-minor-label` still
        //        skip, now on their `text-size`: their property-varying
        //        `interpolate` stops use DIFFERENT symbolrank bucket
        //        threshold sets per zoom stop ([6] at z3 vs. [6, 7] at z6,
        //        and [9] vs. [9, 12] vs. [9, 12, 15]), which entry 12's
        //        `compile_interpolated_data_step_layout_value` refused
        //        because "the corresponding bucket" is undefined when the
        //        thresholds don't line up. That was a separate documented
        //        gap at the time; entry 15 closes it.
        // 14. Data-driven icon names per zoom (this change):
        //    `IconImage::ZoomOfDataIcon` compiles the city labels' real
        //    `icon-image` -- a zoom `step` whose per-zoom value is itself
        //    data-driven (`["case", ["==", ["get", "capital"], 2],
        //    "border-dot-13", ["step", ["get", "symbolrank"], "dot-11", 9,
        //    "dot-10", 11, "dot-9"]]` below z8, `""` from z8 up, where `""`
        //    resolves to NO icon rather than an icon named ""). Entry 13's
        //    text-only degradation was already keeping that shape from
        //    sinking a layer, so this changes NO layer count -- it stays at
        //    176 (measured by running this test, not hand-derived, per the
        //    module's TDD requirement). What it changes is what the two
        //    city-label layers will draw the moment their remaining
        //    `text-size` blocker (entry 13) is lifted: the capital and
        //    settlement dots, instead of bare text.
        //
        //    Out of scope, and refused rather than approximated: a step
        //    keyed on STRING labels (the same trap entry 12 documents for
        //    `settlement-subdivision-label` -- this shape also selects its
        //    bucket by comparing a NUMERIC tag against `>=` thresholds), and
        //    a `case` test that isn't numeric equality against a property.
        //    Also still out of scope, deliberately, are the road shields'
        //    `concat`/`to-string`/`image` icon names; entry 13 already
        //    recovered what mattered there (their text).
        // 15. Threshold-union refinement of property-varying `interpolate`
        //    stops (this change): entry 12 required every stop's `step` to
        //    use IDENTICAL bucket thresholds in identical order, which is
        //    exactly what entry 13 found the two CITY-label layers
        //    violating. `compile_interpolated_data_step_layout_value` now
        //    re-expresses every stop on the UNION of all the stops'
        //    thresholds before mixing them. That rewrite is LOSSLESS, which
        //    is why it is a fix and not an approximation: a `step` is
        //    piecewise-constant, so a threshold it never declared starts a
        //    bucket carrying the value already in effect at that point --
        //    the function is unchanged, only written in more segments (see
        //    `refine_data_step_onto_thresholds` and the sweep test that
        //    proves it). Both city-label layers now compile:
        //      * `settlement-major-label` (stops at z3/6/8/15 declaring the
        //        symbolrank sets [6], [6,7], [9,10], [9,12,15]; union
        //        [6,7,9,10,12,15])
        //      * `settlement-minor-label` (z3/6/8/13, the nested sets [9],
        //        [9,12], [9,12,15], [9,12,15]; union [9,12,15])
        //    Neither has a `match`-shaped paint color, so each compiles
        //    into exactly one `StyleLayer::Symbol` and the total rises to
        //    178 (up from 176, measured by running this test, not
        //    hand-derived, per the module's TDD requirement). They are the
        //    ONLY two layers the relaxation moves: they are the only two
        //    `interpolate`s in the whole vendored style whose stop outputs
        //    are `step`s with disagreeing thresholds (`state-label` and
        //    `country-label` already agreed; every other expression-shaped
        //    stop list is `match`- or arithmetic-shaped, below).
        //
        //    What entry 12 refuses for OTHER reasons keeps being refused,
        //    unchanged, because those are representational limits rather
        //    than alignment ones: `settlement-subdivision-label`'s
        //    `match`-on-STRING stops (encoding them as a `DataStepShape`,
        //    which selects buckets by numeric `>=`, would be silently wrong
        //    rather than merely lossy) and `water-point-label`'s arithmetic
        //    stops. Stops that step on DIFFERENT PROPERTIES also still
        //    `Err`: there is no correspondence between a `symbolrank`
        //    bucket and a `sizerank` one to refine onto.
        // 16. Constant `"line"` placement compiles (this change): entry 7
        //    counted 12 symbol layers as out of scope for using line/
        //    line-center placement. Of those, the 10 that declare the exact
        //    string `"line"` (not `"line-center"`, and not a zoom-`step`)
        //    are now in scope: `tunnel-oneway-arrow-blue`,
        //    `tunnel-oneway-arrow-white`, `road-oneway-arrow-blue`,
        //    `road-oneway-arrow-white`, `bridge-oneway-arrow-blue`,
        //    `bridge-oneway-arrow-white`, `road-label`,
        //    `path-pedestrian-label`, `ferry-aerialway-label`,
        //    `waterway-label`. `natural-line-label` and `water-line-label`
        //    (`"line-center"`) and `road-number-shield` (a zoom-`step`
        //    between `"point"` and `"line"`) stay out of scope and keep
        //    `Err`ing, per this plan's deliberate constant-shapes-only
        //    scope.
        //
        //    Of the 10, 8 newly compile: the 6 oneway-arrow layers (plain
        //    icon-only, no paint color at all) plus `waterway-label` (a
        //    plain, non-`match` `text-color`/`text-halo-color`) each
        //    contribute exactly one `StyleLayer`, for +7; `road-label` and
        //    `path-pedestrian-label` stay skipped on the SAME pre-existing,
        //    unrelated blocker entry 7 already documented (their
        //    `text-size` is a property-varying `interpolate` over `match`
        //    stop outputs, which `LayoutValue` still can't represent).
        //    `ferry-aerialway-label` also newly compiles, and its paint
        //    `text-color` AND `text-halo-color` are both a 2-case `match` on
        //    `class` (`"ferry"` vs. the default), so it cross-products into
        //    2 x 2 = 4 `StyleLayer`s rather than 1, for +4. Total: 6 (arrows)
        //    + 1 (waterway-label) + 4 (ferry-aerialway-label) = 11 new
        //    `StyleLayer`s, raising the total to 178 + 11 = 189.
        // 17. Fix-wave correction (line-placed TEXT-ONLY layers now bail
        //    instead of silently compiling empty): entry 16 above let
        //    `waterway-label` and `ferry-aerialway-label` compile even
        //    though `match_tile_symbols`'s `Line` arm never resolves
        //    `text_field` -- both layers have no `icon-image`, so every
        //    feature they'd ever match hits that arm's `icon.is_none() {
        //    continue; }` guard and produces nothing, forever. That is
        //    silent compilation of something that can never draw, which
        //    contradicts this module's own compiled-vs-skipped taxonomy, so
        //    `compile_symbol_layer` now bails when `placement` resolves to
        //    `Line` at any zoom and `icon_image` is `None`, restoring their
        //    per-layer skip notice.
        //    This removes exactly the two layers entry 16 added for text
        //    alone: `waterway-label` (1 `StyleLayer`) and
        //    `ferry-aerialway-label` (4, its match-color cross-product), for
        //    -5. The 6 arrow layers are untouched (they all carry
        //    `icon-image`). Total: 189 - 5 = 184.
        // 18. Zoom-stepped `symbol-placement` compiles (this change):
        //    `SymbolPlacementValue::ZoomStep` resolves a
        //    `["step", ["zoom"], "point", 11, "line"]` placement per zoom
        //    instead of rejecting it outright, so `road-number-shield` --
        //    entry 16's one remaining zoom-stepped holdout -- now clears its
        //    placement blocker. Its `icon-image` (a `case`/`coalesce`
        //    shield-name shape) and `text-field` (`["get", "ref"]`) already
        //    compile (entries 13/14 and this plan's earlier tasks); its
        //    paint `text-color` is a `case` over two 7-outcome `match`
        //    branches (`shield_text_color_beta` when `shield_text_color_beta`
        //    is set AND its image resolves, else `shield_text_color`), each
        //    branch compiling to 7 `StyleLayer`s, for 14 total -- its paint
        //    block has no `text-halo-color`, so there is no second axis to
        //    cross-product against. `natural-line-label` and
        //    `water-line-label` (`"line-center"`, never a recognized
        //    placement keyword in either shape) stay out of scope and keep
        //    `Err`ing. Total: 184 + 14 = 198.
        // 19. Line-placed TEXT compiling (this change): a line-placed layer
        //    with a `text-field` and no `icon-image` used to be refused at
        //    compile time. It now compiles, and whether it draws is decided
        //    in `match_tile_symbols` instead, so the refusal lives in one
        //    place rather than two. Five more layers clear: 198 + 5 = 203.
        // 20. String-keyed data-driven values (this change, the defect this
        //    plan exists to fix -- Mapbox Streets drew no street names on
        //    any target): `LayoutValue` gains `MatchStepShape` /
        //    `ZoomOfMatchStep`, the string-label sibling of `DataStepShape`/
        //    `ZoomOfDataStep` (see `MatchStepShape`'s doc comment in
        //    `style.rs`), and `compile_interpolated_data_step_layout_value`
        //    dispatches a property-varying `interpolate` to it whenever
        //    every stop's output is a `["match", ["get", key], ...]` on a
        //    STRING tag rather than a `["step", ["get", key], ...]` on a
        //    NUMERIC one (see `compile_match_step_stops`). This was the
        //    LAST blocker on three real layers, all cited verbatim in this
        //    module's `compile_interpolated_data_step_layout_value` doc
        //    comment:
        //      * `road-label` -- its `text-halo-color` is a 2-case `match`
        //        on `class` (`[motorway,trunk]` vs. the default), so it
        //        cross-products into 2 `StyleLayer`s, for +2.
        //      * `path-pedestrian-label` and `settlement-subdivision-label`
        //        -- both have a plain, non-`match` `text-color` AND
        //        `text-halo-color`, so each contributes exactly 1
        //        `StyleLayer`, for +1 apiece.
        //    Total: +4 new `StyleLayer`s, raising the total to 203 + 4 =
        //    207 (measured by running this test, not hand-derived, per the
        //    module's TDD requirement). `water-point-label` stays skipped:
        //    its `text-size` stops are ARITHMETIC
        //    (`["*", ["-", 16, ["sqrt", ["get", "sizerank"]]], 1]`), a
        //    third per-feature shape this change deliberately leaves out of
        //    scope (see `compile_interpolated_data_step_layout_value`'s doc
        //    comment).
        // 21. The dropped label layers (this change). Five more symbol
        //    layers compile, and every one was measured, not derived:
        //      * `natural-line-label` (+1) and `water-line-label` (+2, its
        //        `text-color` is a 2-case `match` on `class`) --
        //        `symbol-placement: "line-center"` is now
        //        `SymbolPlacement::LineCenter`.
        //      * `water-point-label` (+2, the same `text-color` match) --
        //        its arithmetic `text-size` is now
        //        `LayoutValue::FeatureExpression`, evaluated per feature.
        //      * `transit-label` (+6, its `text-color` is a 6-outcome
        //        `match` on `network`) -- its `text-field` is a zoom `step`
        //        of `match`es (`TextField::ZoomStep`/`Match`).
        //      * `airport-label` (+1) -- its `text-field` is a `step` on
        //        `sizerank` over a `concat` with a nested `coalesce`
        //        (`TextField::DataStep`/`ConcatFields`).
        //    Total: 207 + 12 = 219.
        let style = or_skip!(mapbox_streets());
        assert_eq!(
            style.layers.len(),
            219,
            "compiled StyleLayer count for the real vendored Mapbox Streets style changed \
             -- if this is expected (a real compiler improvement or regression), \
             update this assertion and the comment above with verified real numbers \
             (don't guess); if not, something broke",
        );
    }

    #[test]
    fn compiles_the_real_landuse_layer() {
        let style = or_skip!(mapbox_streets());
        let landuse_layers: Vec<_> = style
            .layers
            .iter()
            .filter(|l| matches!(l, StyleLayer::Fill { source_layer, .. } if source_layer == "landuse"))
            .collect();
        // 16 flattened fill-color labels (wood, scrub, agriculture, park,
        // grass, airport, cemetery, glacier, hospital, pitch, sand, school,
        // commercial_area, residential, facility, industrial) + 1 default =
        // 17 fill-color cases, cross-producted with 2 fill-opacity cases
        // (residential vs. default) = 34 (see the comment in
        // compiles_most_of_the_real_vendored_mapbox_streets_style above).
        assert_eq!(
            landuse_layers.len(),
            34,
            "landuse should compile into all 34 real fill-color x fill-opacity StyleLayers"
        );

        // Behavioral check: a real hospital feature with sizerank 20.
        // hospital's class-based filter component (step zoom, false, 11,
        // true) already passes at both zoom 13 and 18, so the difference
        // comes entirely from the arithmetic-comparison filter's
        // zoom-interpolated offset: at zoom 18 the offset is 14 (20 - 14 =
        // 6 <= 14, matches); at zoom 13 the offset is ~0.674 (20 - 0.674 ≈
        // 19.33 > 14, rejected).
        let mut hospital_tags = HashMap::new();
        hospital_tags.insert("class".to_string(), TagValue::String("hospital".to_string()));
        hospital_tags.insert("sizerank".to_string(), TagValue::Number(20.0));
        let tile = tile_with_feature("landuse", hospital_tags, polygon_geometry());

        let fill_op_count = |zoom: u8| {
            match_tile(&tile, &style, zoom)
                .ops
                .iter()
                .filter(|op| matches!(op, PaintOp::Fill { .. }))
                .count()
        };

        assert!(
            fill_op_count(18) > 0,
            "hospital sizerank 20 should be matched by some landuse StyleLayer at zoom 18"
        );
        assert_eq!(
            fill_op_count(13),
            0,
            "hospital sizerank 20 should be matched by no landuse StyleLayer at zoom 13"
        );
    }

    #[test]
    fn compiles_the_liberty_background_color() {
        let style = liberty();
        // Liberty's `background` layer: "background-color": "#f8f4f0".
        let background = style
            .layers
            .iter()
            .find_map(|layer| match layer {
                StyleLayer::Background { color, .. } => Some(color.value_at(12)),
                _ => None,
            })
            .expect("Liberty has a background layer");
        let expected = parse_css_color("#f8f4f0").unwrap();
        assert_eq!(
            (background.r, background.g, background.b, background.a),
            (expected.r, expected.g, expected.b, expected.a)
        );
    }

    /// Liberty's three road-shield symbol layers, verbatim from the vendored
    /// file. Their `icon-image` is a `concat` whose `ref_length` part is a
    /// bare `["get", ...]` on a property that OpenMapTiles types as a Number
    /// (`transportation_name.fields.ref_length` in the live TileJSON), so
    /// this pins both the compiled shape and that it actually resolves
    /// against realistic numeric tags.
    #[test]
    fn compiles_the_liberty_road_shield_icon_names() {
        use crate::style::{ConcatPart, IconImage};

        let style = liberty();
        let concat_parts = |id: &str| -> Vec<ConcatPart> {
            let icon = style
                .layers
                .iter()
                .find_map(|layer| match layer {
                    StyleLayer::Symbol { id: layer_id, icon_image, .. } if layer_id == id => {
                        Some(icon_image.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Liberty has a symbol layer {id:?}"))
                .unwrap_or_else(|| panic!("Liberty layer {id:?} has an icon-image"));
            match icon {
                IconImage::Concat(parts) => parts,
                other => panic!("expected a concat icon-image for {id:?}, got {other:?}"),
            }
        };

        // ["concat", "road_", ["get", "ref_length"]]
        assert_eq!(
            concat_parts("highway-shield-non-us"),
            vec![ConcatPart::Literal("road_".into()), ConcatPart::Get("ref_length".into())]
        );
        // ["concat", ["get", "network"], "_", ["get", "ref_length"]] -- twice.
        let network_shape = vec![
            ConcatPart::Get("network".into()),
            ConcatPart::Literal("_".into()),
            ConcatPart::Get("ref_length".into()),
        ];
        assert_eq!(concat_parts("highway-shield-us-interstate"), network_shape);
        assert_eq!(concat_parts("road_shield_us"), network_shape);

        // And they resolve against the numeric `ref_length` real tiles carry.
        let mut tags = HashMap::new();
        tags.insert("network".to_string(), TagValue::String("us-interstate".to_string()));
        tags.insert("ref_length".to_string(), TagValue::Number(2.0));
        assert_eq!(
            IconImage::Concat(concat_parts("highway-shield-non-us")).resolve(&tags, 12).as_deref(),
            Some("road_2")
        );
        assert_eq!(
            IconImage::Concat(concat_parts("highway-shield-us-interstate"))
                .resolve(&tags, 12)
                .as_deref(),
            Some("us-interstate_2")
        );
    }

    /// Liberty's four POI symbol layers -- `poi_transit` (the bus stops) and
    /// `poi_r1`/`poi_r7`/`poi_r20` -- verbatim from the vendored file. All
    /// four used to name a sprite with a shape `compile_icon_image` refused,
    /// so all four compiled text-only and drew no icon at all.
    #[test]
    fn compiles_the_liberty_poi_icon_names() {
        use crate::style::IconImage;

        let style = liberty();
        let icon_of = |id: &str| -> IconImage {
            style
                .layers
                .iter()
                .find_map(|layer| match layer {
                    StyleLayer::Symbol { id: layer_id, icon_image, .. } if layer_id == id => {
                        Some(icon_image.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Liberty has a symbol layer {id:?}"))
                .unwrap_or_else(|| panic!("Liberty layer {id:?} kept its icon-image"))
        };

        // `poi_transit`: ["to-string", ["get", "class"]]
        let transit = icon_of("poi_transit");
        assert!(matches!(&transit, IconImage::ToStringGet(k) if k == "class"));
        let mut bus_stop = HashMap::new();
        bus_stop.insert("class".to_string(), TagValue::String("bus".to_string()));
        assert_eq!(transit.resolve(&bus_stop, 17).as_deref(), Some("bus"));

        // `poi_r1`/`poi_r7`/`poi_r20`:
        //   ["match", ["get", "subclass"], ["florist", "furniture"],
        //    ["get", "subclass"], ["get", "class"]]
        for id in ["poi_r1", "poi_r7", "poi_r20"] {
            let icon = icon_of(id);
            match &icon {
                IconImage::MatchGet { key, arms, fallback } => {
                    assert_eq!(key, "subclass", "{id}");
                    assert_eq!(arms.len(), 1, "{id}");
                    assert_eq!(arms[0].0, vec!["florist".to_string(), "furniture".to_string()], "{id}");
                    assert!(matches!(&arms[0].1, IconImage::Get(k) if k == "subclass"), "{id}");
                    assert!(matches!(&**fallback, IconImage::Get(k) if k == "class"), "{id}");
                }
                other => panic!("expected a match icon-image for {id:?}, got {other:?}"),
            }

            let mut florist = HashMap::new();
            florist.insert("subclass".to_string(), TagValue::String("florist".to_string()));
            florist.insert("class".to_string(), TagValue::String("shop".to_string()));
            assert_eq!(icon.resolve(&florist, 17).as_deref(), Some("florist"), "{id}");

            let mut cafe = HashMap::new();
            cafe.insert("subclass".to_string(), TagValue::String("cafe".to_string()));
            cafe.insert("class".to_string(), TagValue::String("restaurant".to_string()));
            assert_eq!(icon.resolve(&cafe, 17).as_deref(), Some("restaurant"), "{id}");

            assert_eq!(icon.resolve(&HashMap::new(), 17), None, "{id}");
        }
    }

    /// The startup notice this pins is a real one the Liberty demo printed:
    ///
    ///     style_spec: layer "poi_transit": dropping unsupported
    ///     "icon-image", compiling as text-only: ...
    ///
    /// It is emitted from exactly one place -- `compile_symbol_layer`'s
    /// `Err(..) if text_field.is_some()` arm -- and that same arm is what
    /// sets the layer's `icon_image` to `None`. So "every Liberty layer whose
    /// JSON declares an `icon-image` still has one once compiled" is the same
    /// assertion as "no layer printed that notice", without reaching for
    /// stderr. Written over the whole file rather than the four known ids so
    /// that a future style-file update cannot quietly reintroduce it
    /// elsewhere.
    #[test]
    fn no_liberty_layer_drops_its_icon_image_to_the_text_only_fallback() {
        let bytes = std::fs::read("examples/liberty-style.json").expect("vendored style file");
        let root: Value = serde_json::from_slice(&bytes).expect("vendored style is valid JSON");
        let declared: Vec<String> = root["layers"]
            .as_array()
            .expect("style has a layers array")
            .iter()
            .filter(|layer| layer["layout"].get("icon-image").is_some())
            .map(|layer| layer["id"].as_str().expect("every layer has an id").to_string())
            .collect();
        assert!(!declared.is_empty(), "the vendored Liberty style declares icon-images");

        let style = liberty();
        let dropped: Vec<&String> = declared
            .iter()
            .filter(|id| {
                style.layers.iter().any(|layer| {
                    matches!(layer, StyleLayer::Symbol { id: compiled, icon_image: None, .. } if compiled == *id)
                })
            })
            .collect();
        assert!(
            dropped.is_empty(),
            "these Liberty layers declare an icon-image but compiled without one \
             (each prints a \"dropping unsupported icon-image\" notice): {dropped:?}"
        );
    }

    #[test]
    fn compiles_a_match_icon_image_with_single_value_labels_and_several_arms() {
        use crate::style::IconImage;

        // A label written as a bare value rather than a one-element array,
        // which the style spec allows just as readily -- and more than one
        // label/output pair, which is what `match` is for.
        let json = serde_json::json!([
            "match",
            ["get", "class"],
            "bus",
            "transit",
            ["ferry", "pier"],
            ["get", "subclass"],
            "dot"
        ]);
        let icon = compile_icon_image(&json).unwrap();
        match &icon {
            IconImage::MatchGet { key, arms, fallback } => {
                assert_eq!(key, "class");
                assert_eq!(arms.len(), 2);
                assert_eq!(arms[0].0, vec!["bus".to_string()]);
                assert!(matches!(&arms[0].1, IconImage::Literal(s) if s == "transit"));
                assert_eq!(arms[1].0, vec!["ferry".to_string(), "pier".to_string()]);
                assert!(matches!(&arms[1].1, IconImage::Get(k) if k == "subclass"));
                assert!(matches!(&**fallback, IconImage::Literal(s) if s == "dot"));
            }
            other => panic!("expected a match icon-image, got {other:?}"),
        }

        let one = |class: &str| {
            let mut tags = HashMap::new();
            tags.insert("class".to_string(), TagValue::String(class.to_string()));
            tags.insert("subclass".to_string(), TagValue::String("harbor".to_string()));
            icon.resolve(&tags, 14)
        };
        assert_eq!(one("bus").as_deref(), Some("transit"));
        assert_eq!(one("pier").as_deref(), Some("harbor"));
        assert_eq!(one("cafe").as_deref(), Some("dot"));
    }

    #[test]
    fn rejects_a_match_icon_image_this_module_cannot_represent() {
        // A numeric label: out of scope rather than silently ignored, since
        // an ignored label hands every feature the fallback icon while
        // looking like it worked.
        assert!(compile_icon_image(&serde_json::json!(["match", ["get", "rank"], 1, "big", "small"])).is_err());
        // No fallback -- the style spec requires one, and without it the last
        // output would be misread as the fallback.
        assert!(compile_icon_image(&serde_json::json!(["match", ["get", "class"], "bus", "transit"])).is_err());
        // Matching on something other than a property lookup.
        assert!(compile_icon_image(&serde_json::json!(["match", ["zoom"], 5, "small", "big"])).is_err());
    }

    #[test]
    fn compiles_a_real_layers_filter_and_zoom_interpolated_width() {
        let style = liberty();
        // `road_motorway` (source-layer "transportation"):
        //   filter: all(geometry-type LineString, brunnel not bridge/tunnel,
        //               class in [motorway], ramp != 1)
        //   line-width: interpolate(exponential 1.2, zoom, 5,0, 6,0.5, ...)
        // Find the line layer over "transportation" whose filter accepts a
        // plain motorway and whose width grows with zoom.
        let motorway = tags(&[("class", "motorway")]);
        let matching: Vec<_> = style
            .layers
            .iter()
            .filter_map(|layer| match layer {
                StyleLayer::Line { source_layer, filter, width, .. }
                    if source_layer == "transportation" && filter.matches(&motorway, 14.0) =>
                {
                    Some(width.clone())
                }
                _ => None,
            })
            .collect();
        assert!(!matching.is_empty(), "no motorway line layer compiled");
        assert!(
            matching.iter().any(|w| w.value_at(14) > w.value_at(8)),
            "expected at least one motorway width that grows with zoom"
        );

        // A bridge/tunnel motorway must NOT be accepted by those same
        // non-brunnel layers -- proves the layer filter really compiled in
        // (and wasn't flattened away by the geometry-type clause handling).
        let tunnel_motorway = tags(&[("class", "motorway"), ("brunnel", "tunnel")]);
        let road_motorway_accepts_tunnels = style.layers.iter().any(|layer| match layer {
            StyleLayer::Line { source_layer, filter, min_zoom, .. } => {
                source_layer == "transportation"
                    && *min_zoom == Some(5)
                    && filter.matches(&tunnel_motorway, 14.0)
            }
            _ => false,
        });
        assert!(!road_motorway_accepts_tunnels);
    }

    /// `line-gap-width` end to end, over the style this renderer actually
    /// draws: the eight `*-case` layers of `examples/mapbox-style.json`
    /// each carry a zoom-ramped gap, and each gap is the width of the FILL
    /// layer it casings, taken verbatim from the same pair of layers in the
    /// vendored `examples/mapbox-streets-style.json`.
    ///
    /// That last equality is the point of the whole property and the reason
    /// this test compares two layers rather than asserting a number. A
    /// casing hugs its fill only if `case.gap == fill.width` at EVERY zoom;
    /// the wider-under-line workaround this replaced could match at one
    /// zoom and drift at the next, because nothing tied the two ramps
    /// together. Here they are the same ramp.
    #[test]
    fn every_road_casing_gaps_by_exactly_the_width_of_the_road_it_encloses() {
        let bytes = mapbox_fixture_or_skip!("mapbox-style.json");
        let style = parse_and_compile(&bytes).expect("the demo style must compile");
        let line = |id: &str| {
            style
                .layers
                .iter()
                .find_map(|l| match l {
                    StyleLayer::Line { id: lid, width, gap_width, cap, join, .. } if lid == id => {
                        Some((width.clone(), gap_width.clone(), *cap, *join))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no line layer {id:?}"))
        };

        let mut checked = 0;
        for prefix in ["road", "tunnel"] {
            for class in ["street", "secondary-tertiary", "primary", "motorway-trunk"] {
                let (case_width, case_gap, cap, join) = line(&format!("{prefix}-{class}-case"));
                let (fill_width, fill_gap, _, _) = line(&format!("{prefix}-{class}"));
                for zoom in 3..=22u8 {
                    assert_eq!(
                        case_gap.value_at(zoom),
                        fill_width.value_at(zoom),
                        "{prefix}-{class}: the casing's gap must equal the road's width at z{zoom}"
                    );
                }
                assert_eq!(fill_gap.value_at(15), 0.0, "{prefix}-{class} is a fill, not a casing");
                assert!(
                    case_width.value_at(15) > 0.0 && case_width.value_at(15) < 2.0,
                    "{prefix}-{class}-case: the casing is a thin stroke either side, not an under-line \
                     (got {})",
                    case_width.value_at(15)
                );
                // ...and the two layout properties came through with it.
                assert_eq!((cap, join), (crate::style::LineCap::Round, crate::style::LineJoin::Round));
                checked += 1;
            }
        }
        assert_eq!(checked, 8, "the style's eight casing layers");
    }

    /// `layout.line-cap`/`line-join`: a literal keyword compiles, an absent
    /// one takes the style spec's default, and an EXPRESSION takes the
    /// default too rather than being resolved at one zoom and left stale at
    /// every other -- see `style::LineCap` for why that trade is made
    /// deliberately, and `map_view::ops_share_geometry_set` for the cache
    /// invariant it protects.
    #[test]
    fn a_literal_cap_and_join_compile_and_an_expression_falls_back_to_the_default() {
        use crate::style::{LineCap, LineJoin};
        let compile = |layout: serde_json::Value| {
            let json = serde_json::json!({
                "layers": [{
                    "id": "road", "type": "line", "source-layer": "transportation",
                    "layout": layout,
                    "paint": {"line-color": "#000000", "line-width": 2},
                }]
            });
            let style = parse_and_compile(json.to_string().as_bytes()).expect("must compile");
            match &style.layers[0] {
                StyleLayer::Line { cap, join, .. } => (*cap, *join),
                other => panic!("expected a line layer, got {other:?}"),
            }
        };

        assert_eq!(compile(serde_json::json!({})), (LineCap::Butt, LineJoin::Miter), "spec defaults");
        assert_eq!(
            compile(serde_json::json!({"line-cap": "round", "line-join": "round"})),
            (LineCap::Round, LineJoin::Round)
        );
        assert_eq!(
            compile(serde_json::json!({"line-cap": "square", "line-join": "bevel"})),
            (LineCap::Square, LineJoin::Bevel)
        );
        // The shape 31 of the vendored Mapbox Streets document's layers use.
        assert_eq!(
            compile(serde_json::json!({
                "line-cap": ["step", ["zoom"], "butt", 14, "round"],
                "line-join": ["step", ["zoom"], "miter", 14, "round"],
            })),
            (LineCap::Butt, LineJoin::Miter),
            "a zoom-stepped cap/join degrades to the spec default, with the layer kept"
        );
        // A keyword no cap/join names is the same outcome, not a hard error.
        assert_eq!(
            compile(serde_json::json!({"line-cap": "flat", "line-join": "rounded"})),
            (LineCap::Butt, LineJoin::Miter)
        );
    }

    #[test]
    fn defaults_a_missing_line_width_to_the_spec_default_of_one() {
        // `park_outline` is the one real Liberty line layer that declares
        // `line-color` but no `line-width`.
        let style = liberty();
        let park_outline_width = style
            .layers
            .iter()
            .find_map(|layer| match layer {
                StyleLayer::Line { source_layer, width, .. } if source_layer == "park" => {
                    Some(width.clone())
                }
                _ => None,
            })
            .expect("Liberty has a park_outline line layer");
        assert_eq!(park_outline_width.value_at(12), 1.0);
    }

    #[test]
    fn drops_a_no_op_filter_instead_of_nesting_it() {
        // Liberty's `park` fill layer has no "filter" at all and a bare
        // literal `fill-color`, so both the layer filter and the paint
        // expression's case filter are `Filter::Any`; the compiled layer
        // must carry a bare `Any`, not `All([Any, Any])`.
        let style = liberty();
        let park = style
            .layers
            .iter()
            .find_map(|layer| match layer {
                StyleLayer::Fill { source_layer, filter, .. } if source_layer == "park" => {
                    Some(filter.clone())
                }
                _ => None,
            })
            .expect("Liberty has a park fill layer");
        assert!(matches!(park, Filter::Any), "expected bare Any, got {park:?}");
    }

    #[test]
    fn preserves_the_json_layer_order_as_draw_order() {
        let style = liberty();
        // Liberty's first layer is the background; water fills come well
        // before the road lines that must draw on top of them.
        assert!(matches!(style.layers[0], StyleLayer::Background { .. }));
        let first_water = style
            .layers
            .iter()
            .position(|l| matches!(l, StyleLayer::Fill { source_layer, .. } if source_layer == "water"))
            .expect("water fill");
        let first_road = style
            .layers
            .iter()
            .position(|l| matches!(l, StyleLayer::Line { source_layer, .. } if source_layer == "transportation"))
            .expect("transportation line");
        assert!(first_water < first_road);
    }

    /// The real Liberty `building` layer, which declares
    /// `fill-outline-color` as a zoom `interpolate` -- the shape that
    /// matters, because an outline is what separates one footprint from the
    /// next at building zoom, and a constant would not exercise the
    /// zoom-value path at all.
    #[test]
    fn a_fill_outline_color_compiles_onto_the_layer_that_declares_it() {
        let style = liberty();
        let outline = style
            .layers
            .iter()
            .find_map(|layer| match layer {
                StyleLayer::Fill { id, outline_color, .. } if id == "building" => Some(outline_color),
                _ => None,
            })
            .expect("Liberty has a building fill layer")
            .as_ref()
            .expect("Liberty's building layer declares fill-outline-color");

        // `hsla(35,6%,79%,0.32)` at z13 interpolating to an opaque
        // `hsl(35,6%,79%)` at z14: the alpha is what proves the value was
        // read as a real zoom expression rather than snapped to one end.
        let at_13 = outline.value_at(13);
        let at_14 = outline.value_at(14);
        assert!(
            (at_13.a - 0.32).abs() < 0.01,
            "z13 outline alpha should be 0.32, got {}",
            at_13.a
        );
        assert!((at_14.a - 1.0).abs() < 0.01, "z14 outline alpha should be 1.0, got {}", at_14.a);
        assert!(
            at_13.r > 0.7 && (at_13.r - at_14.r).abs() < 0.01,
            "hue/lightness are equal at both stops; only alpha interpolates"
        );
    }

    /// The spec says an unspecified `fill-outline-color` defaults to
    /// `fill-color`. This renderer deliberately compiles that default to NO
    /// outline instead -- see `StyleLayer::Fill`'s `outline_color` for the
    /// full reasoning (an outline the exact colour of the fill beneath it is
    /// invisible over an opaque fill and double-blends over a translucent
    /// one, and it would cost a stroked copy of every polygon in the tile).
    /// Pinned because it is a considered divergence from MapLibre, not an
    /// oversight: if someone later "fixes" it to match the spec text
    /// literally, this test says why that is the wrong fix.
    #[test]
    fn a_fill_with_no_outline_color_compiles_to_no_outline_rather_than_the_fill_colour() {
        let json = br##"{"layers": [{
            "id": "landuse", "type": "fill", "source-layer": "landuse",
            "paint": {"fill-color": "#abc"}
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        let StyleLayer::Fill { outline_color, .. } = &style.layers[0] else {
            panic!("expected a Fill layer");
        };
        assert!(outline_color.is_none(), "an undeclared fill-outline-color must not become the fill colour");
    }

    /// `fill-antialias: false` is MapLibre's switch for "draw no outline"
    /// (`draw_fill.ts`'s `drawOutline` returns before drawing when it is
    /// false), and it wins over a declared `fill-outline-color`.
    #[test]
    fn fill_antialias_false_suppresses_a_declared_fill_outline_color() {
        let json = br##"{"layers": [{
            "id": "landcover", "type": "fill", "source-layer": "landcover",
            "paint": {"fill-color": "#abc", "fill-outline-color": "#f00", "fill-antialias": false}
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        let StyleLayer::Fill { outline_color, .. } = &style.layers[0] else {
            panic!("expected a Fill layer");
        };
        assert!(outline_color.is_none(), "fill-antialias: false must suppress the outline");

        // ... and the same layer WITH antialiasing left at its default
        // keeps the outline, so the assertion above is testing the flag and
        // not some unrelated reason the property went missing.
        let json = br##"{"layers": [{
            "id": "landcover", "type": "fill", "source-layer": "landcover",
            "paint": {"fill-color": "#abc", "fill-outline-color": "#f00"}
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        let StyleLayer::Fill { outline_color, .. } = &style.layers[0] else {
            panic!("expected a Fill layer");
        };
        let outline = outline_color.as_ref().expect("outline kept when fill-antialias is defaulted");
        let colour = outline.value_at(14);
        assert!(colour.r > 0.99 && colour.g < 0.01, "outline should be #f00, got {colour:?}");
    }

    /// `fill-extrusion` has no `fill-outline-color` in the style spec, and
    /// its flat footprint below `EXTRUSION_MIN_ZOOM` shares
    /// `compile_flat_fill_cases` with plain fills. That flattened
    /// `StyleLayer::Fill` must therefore never acquire an outline --
    /// including from a key a style has no business setting on it.
    #[test]
    fn a_flattened_fill_extrusion_never_acquires_an_outline() {
        let json = br##"{"layers": [{
            "id": "building-3d", "type": "fill-extrusion", "source-layer": "building",
            "paint": {"fill-extrusion-color": "#abc", "fill-outline-color": "#f00"}
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        let Some(StyleLayer::Fill { outline_color, .. }) = style.layers.iter().find(|l| matches!(l, StyleLayer::Fill { .. })) else {
            panic!("a fill-extrusion with no minzoom keeps a flat Fill below the extrusion floor");
        };
        assert!(outline_color.is_none(), "fill-extrusion has no outline property in the spec");
    }

    #[test]
    fn expands_a_data_driven_fill_color_into_one_layer_per_case() {
        // Synthetic (no Liberty paint property uses `match`): a
        // feature-dependent fill-color must expand into one StyleLayer per
        // case, each ANDed with the layer's own filter.
        let json = br##"{"layers": [{
            "id": "landuse", "type": "fill", "source-layer": "landuse",
            "filter": ["==", ["get", "visible"], "yes"],
            "paint": {"fill-color": ["match", ["get", "class"], ["park"], "#0f0", "#ccc"]}
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(style.layers.len(), 2);

        let park = tags(&[("class", "park"), ("visible", "yes")]);
        let hidden_park = tags(&[("class", "park"), ("visible", "no")]);
        let other = tags(&[("class", "school"), ("visible", "yes")]);

        let color_for = |t: &HashMap<String, TagValue>| {
            style
                .layers
                .iter()
                .find_map(|layer| match layer {
                    StyleLayer::Fill { filter, color, .. } if filter.matches(t, 14.0) => {
                        Some(color.value_at(10))
                    }
                    _ => None,
                })
        };
        let green = parse_css_color("#0f0").unwrap();
        let park_color = color_for(&park).expect("park matches a compiled case");
        assert_eq!((park_color.r, park_color.g, park_color.b), (green.r, green.g, green.b));
        let other_color = color_for(&other).expect("non-park matches the default case");
        assert!(other_color.g < green.g);
        // The layer's own filter still applies to every expanded case.
        assert!(color_for(&hidden_park).is_none());
    }

    #[test]
    fn cross_products_a_data_driven_line_color_with_a_data_driven_line_width() {
        // Synthetic (Liberty never needs this): both line-color and
        // line-width are `match` expressions, so every color case has to be
        // paired with every width case, with both cases' filters ANDed.
        let json = br##"{"layers": [{
            "id": "roads", "type": "line", "source-layer": "transportation",
            "paint": {
                "line-color": ["match", ["get", "class"], ["motorway"], "#f00", "#00f"],
                "line-width": ["match", ["get", "brunnel"], ["bridge"], 4, 2]
            }
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(style.layers.len(), 4, "2 color cases x 2 width cases");

        let for_tags = |t: &HashMap<String, TagValue>| {
            let matched: Vec<_> = style
                .layers
                .iter()
                .filter_map(|layer| match layer {
                    StyleLayer::Line { filter, color, width, .. } if filter.matches(t, 14.0) => {
                        Some((color.value_at(10), width.value_at(10)))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(matched.len(), 1, "exactly one cross-product cell must match {t:?}");
            matched[0]
        };

        let red = parse_css_color("#f00").unwrap();
        let (color, width) = for_tags(&tags(&[("class", "motorway"), ("brunnel", "bridge")]));
        assert_eq!((color.r, color.g, color.b), (red.r, red.g, red.b));
        assert_eq!(width, 4.0);

        let (color, width) = for_tags(&tags(&[("class", "minor")]));
        assert!(color.b > color.r, "non-motorway must get the blue default");
        assert_eq!(width, 2.0);
    }

    #[test]
    fn accepts_a_fractional_zoom_bound_but_still_rejects_an_out_of_range_one() {
        // A fractional bound used to be a hard error on EVERY layer type,
        // which `continent-label`'s real `"minzoom": 0.75` tripped. It now
        // widens instead (floor the min, ceil the max) -- see `zoom_bound`.
        let json = br##"{"layers": [{
            "id": "x", "type": "fill", "source-layer": "landuse",
            "minzoom": 12.5, "maxzoom": 14.5, "paint": {"fill-color": "#fff"}
        }]}"##;
        let style = parse_and_compile(json).expect("a fractional zoom bound must compile");
        let StyleLayer::Fill { min_zoom, max_zoom, .. } = &style.layers[0] else {
            panic!("expected a Fill layer");
        };
        assert_eq!((*min_zoom, *max_zoom), (Some(12), Some(15)), "min floors, max ceils");

        // Out of range is still a hard error, and still cites the layer id.
        let json = br##"{"layers": [{
            "id": "x", "type": "fill", "source-layer": "landuse",
            "minzoom": 42, "paint": {"fill-color": "#fff"}
        }]}"##;
        let error = parse_and_compile(json).unwrap_err().to_string();
        assert!(error.contains("\"x\""), "error should cite the layer id: {error}");
    }

    #[test]
    fn rejects_a_fill_layer_without_a_source_layer() {
        let json = br##"{"layers": [{"id": "x", "type": "fill", "paint": {"fill-color": "#fff"}}]}"##;
        assert!(parse_and_compile(json).is_err());
    }

    #[test]
    fn rejects_a_document_without_a_layers_array() {
        assert!(parse_and_compile(br#"{"version": 8}"#).is_err());
    }

    #[test]
    fn skips_a_layer_whose_filter_fails_to_compile_and_keeps_the_rest() {
        // "within" is not a supported filter operator (see
        // `filter::compile_filter`'s match arms), so `broken`'s filter
        // compilation fails; `fine` is a perfectly valid fill layer. The
        // whole style must still compile, with `broken` simply absent from
        // the result rather than the whole call returning `Err`.
        let json = br##"{"layers": [
            {
                "id": "broken", "type": "fill", "source-layer": "landuse",
                "filter": ["within", {"type": "Polygon", "coordinates": []}],
                "paint": {"fill-color": "#f00"}
            },
            {
                "id": "fine", "type": "fill", "source-layer": "water",
                "paint": {"fill-color": "#00f"}
            }
        ]}"##;
        let style = parse_and_compile(json).expect("one bad layer must not fail the whole style");
        assert_eq!(style.layers.len(), 1, "only the valid layer should compile");
        let fine_present = style
            .layers
            .iter()
            .any(|l| matches!(l, StyleLayer::Fill { source_layer, .. } if source_layer == "water"));
        assert!(fine_present, "the valid 'fine' layer must be present");
        let broken_present = style
            .layers
            .iter()
            .any(|l| matches!(l, StyleLayer::Fill { source_layer, .. } if source_layer == "landuse"));
        assert!(!broken_present, "the broken 'broken' layer must be absent, not present as a default");
    }

    /// `buildings-3d` below carries no `fill-extrusion-color`, so since the
    /// flat-footprint fallback it is skipped by the pattern-fill rule
    /// rather than by its layer type -- the same rule, for the same reason,
    /// that skips a `fill-pattern`-only `fill`. Left in this test verbatim
    /// to pin exactly that: a colourless extrusion must not start drawing
    /// black.
    #[test]
    fn skips_unsupported_layer_types_without_erroring() {
        // `heatmap`, not the `raster` this test used to name. Raster is a
        // compiled layer type now, so a raster layer here would be testing
        // that a SUPPORTED type compiles -- which is a different claim,
        // made by `a_raster_layer_compiles_with_its_own_opacity_curve`.
        // The type named here has to be one the gate still turns away, or
        // this test passes while asserting nothing.
        let json = br##"{"layers": [
            {"id": "labels", "type": "symbol", "source-layer": "place"},
            {"id": "hills", "type": "heatmap", "source": "ne2"},
            {"id": "buildings-3d", "type": "fill-extrusion", "source-layer": "building"},
            {"id": "water", "type": "fill", "source-layer": "water", "paint": {"fill-color": "#00f"}}
        ]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(style.layers.len(), 1);
    }

    /// The layer that motivated raster support, compiled from the vendored
    /// Liberty style's own bytes rather than from a fixture written to
    /// pass.
    ///
    /// `natural_earth` is Liberty's shaded-relief basemap and it was the
    /// one layer of 111 that the type gate turned away, with
    /// `style_spec: skipping layer "natural_earth": unsupported layer type
    /// "raster"` on stderr at every single compile.
    ///
    /// The two opacity endpoints are the claim worth grading. Liberty
    /// writes `["interpolate", ["exponential", 1.5], ["zoom"], 0, 0.6, 6,
    /// 0.1]` -- relief that is strongest when the whole globe is in view
    /// and fades away as the vector layers take over. A raster layer that
    /// compiled but pinned its opacity to the style-spec default of 1
    /// would draw a hillshade at full strength over every city, which is
    /// the kind of wrong that looks deliberate. Reading both ends is what
    /// separates "the expression compiled" from "the expression is
    /// constant".
    #[test]
    fn a_raster_layer_compiles_with_its_own_opacity_curve() {
        let bytes = std::fs::read("examples/liberty-style.json").expect("run from the repo root");
        let style = parse_and_compile(&bytes).expect("Liberty compiles");

        let rasters: Vec<&StyleLayer> =
            style.layers.iter().filter(|l| matches!(l, StyleLayer::Raster { .. })).collect();
        assert_eq!(
            rasters.len(),
            1,
            "Liberty declares exactly one raster layer; got {:?}",
            rasters.iter().map(|l| l.id()).collect::<Vec<_>>()
        );

        let StyleLayer::Raster { id, source, opacity, min_zoom, max_zoom } = rasters[0] else {
            unreachable!("filtered on the variant above")
        };
        assert_eq!(id, "natural_earth");
        assert_eq!(source, "ne2_shaded", "the layer must carry the source it names");
        assert_eq!(*min_zoom, None);
        assert_eq!(*max_zoom, Some(7), "Liberty caps this layer at z7");

        // The two ends of the style's own interpolation. Exact, not
        // approximate: both are written as literals in the document and
        // both are exactly representable in f32, so a tolerance here would
        // only be hiding something.
        assert_eq!(opacity.value_at(0), 0.6, "z0 is the strong end of the relief");
        assert_eq!(opacity.value_at(6), 0.1, "z6 is the faded end");
        assert_ne!(
            opacity.value_at(0),
            opacity.value_at(6),
            "a constant opacity here would mean the expression was dropped, not compiled"
        );
    }

    /// The whole point of the fallback, on the shape that motivated it: a
    /// style that hands over from a flat `fill` to a `fill-extrusion` at a
    /// zoom threshold, exactly as Liberty's `building`/`building-3d` pair
    /// does. Before this, z14 had no buildings at all.
    #[test]
    fn a_fill_extrusion_layer_draws_its_footprint_flat_instead_of_vanishing() {
        let json = br##"{"layers": [
            {"id": "building", "type": "fill", "source-layer": "building", "maxzoom": 14,
             "paint": {"fill-color": "#dfdbd7"}},
            {"id": "building-3d", "type": "fill-extrusion", "source-layer": "building", "minzoom": 14,
             "paint": {"fill-extrusion-color": "#0000ff", "fill-extrusion-opacity": 0.8,
                       "fill-extrusion-height": ["get", "render_height"],
                       "fill-extrusion-base": ["get", "render_min_height"]}}
        ]}"##;
        let style = parse_and_compile(json).unwrap();
        let [_flat, StyleLayer::Extrusion { id, source_layer, color, opacity, min_zoom, max_zoom, .. }] =
            style.layers.as_slice()
        else {
            panic!("expected the extrusion to compile to an Extrusion after the flat Fill, got {:?}", style.layers)
        };
        assert_eq!(id, "building-3d");
        assert_eq!(source_layer, "building");
        // Its OWN colour and opacity, not the flat layer's.
        assert_eq!(color.value_at(14), Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 });
        assert_eq!(opacity.value_at(14), 0.8);
        assert_eq!((*min_zoom, *max_zoom), (Some(14), None));
    }

    /// Height is read now, and reading it must survive the shapes real
    /// styles write it in. Both vendored `fill-extrusion-height` forms are
    /// feature-driven expressions the constant folder cannot reduce
    /// (`["get", ...]` in Liberty, `["match", ["get", "type"], ...]` in
    /// Mapbox Standard). `compile_extrusion_value` keeps those whole and
    /// degrades anything it cannot evaluate to the default; if it ever
    /// failed the layer instead, the buildings would vanish again -- the
    /// bug the flat fallback first existed to fix, back through the door.
    #[test]
    fn an_unfoldable_height_expression_cannot_cost_the_layer_its_footprint() {
        let json = br##"{"layers": [
            {"id": "building-3d", "type": "fill-extrusion", "source-layer": "building",
             "paint": {"fill-extrusion-color": "#abc",
                       "fill-extrusion-height": ["match", ["get", "type"], "tower", 200, ["get", "render_height"]],
                       "fill-extrusion-vertical-scale": ["get", "nonsense"],
                       "fill-extrusion-flood-light-intensity": ["get", "also-nonsense"]}}
        ]}"##;
        let style = parse_and_compile(json).unwrap();
        // With no `minzoom`, the layer also keeps its flat footprint below
        // `EXTRUSION_MIN_ZOOM`, so it compiles to one extrusion and one flat
        // fill -- never to nothing.
        assert_eq!(style.layers.len(), 2, "an unreadable height must not skip the layer: {:?}", style.layers);
        // And it stays ONE extrusion: an ignored property must not join the
        // colour/opacity cross-product either.
        let extrusions: Vec<_> = style.layers.iter().filter(|l| matches!(l, StyleLayer::Extrusion { .. })).collect();
        assert!(matches!(extrusions.as_slice(), [StyleLayer::Extrusion { id, .. }] if id == "building-3d"));
    }

    /// The Standard fixture under `preset`, with its 3D objects switched
    /// on: the vendored import sets `show3dObjects: false`, which hides
    /// `3d-building` (its `visibility` is a `["config", ...]` case).
    fn standard_fixture_with_3d_under_preset(preset: &str) -> Option<Style> {
        standard_fixture_with_3d_under(preset, None)
    }

    /// The same, optionally overriding the import's `theme` (the fixture's
    /// own is `monochrome`).
    fn standard_fixture_with_3d_under(preset: &str, theme: Option<&str>) -> Option<Style> {
        let bytes = crate::mapbox_fixtures::read("mapbox-standard-dark-style.json")?;
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        root["imports"][0]["config"]["lightPreset"] = serde_json::json!(preset);
        root["imports"][0]["config"]["show3dObjects"] = serde_json::json!(true);
        if let Some(theme) = theme {
            root["imports"][0]["config"]["theme"] = serde_json::json!(theme);
        }
        Some(parse_and_compile(root.to_string().as_bytes()).expect("the fixture must compile"))
    }

    fn extrusions(style: &Style) -> Vec<&StyleLayer> {
        style.layers.iter().filter(|l| matches!(l, StyleLayer::Extrusion { .. })).collect()
    }

    #[test]
    fn liberty_building_3d_compiles_to_an_extrusion_with_feature_heights() {
        let style = liberty();
        let layers = extrusions(&style);
        assert_eq!(layers.len(), 1, "Liberty has one fill-extrusion layer with a constant colour");
        let StyleLayer::Extrusion { id, height, base, opacity, vertical_gradient, min_zoom, group, .. } = layers[0] else { unreachable!() };
        assert_eq!(id, "building-3d");
        assert_eq!(*height, crate::style::ExtrusionValue::Get("render_height".into()));
        assert_eq!(*base, crate::style::ExtrusionValue::Get("render_min_height".into()));
        assert_eq!(opacity.value_at(16), 0.8);
        assert!(*vertical_gradient);
        assert_eq!(*min_zoom, Some(14));
        assert_eq!(*group, 0);
        // minzoom 14 >= 12: no flat fallback.
        assert!(!style.layers.iter().any(|l| matches!(l, StyleLayer::Fill { id, .. } if id == "building-3d")));
    }

    #[test]
    fn standards_3d_building_height_evaluates_per_feature() {
        use crate::mvt::TagValue;
        let style = or_skip!(standard_fixture_with_3d_under_preset("night"));
        let layer = style.layers.iter().find(|l| l.id() == "3d-building").expect("3d-building compiles");
        let StyleLayer::Extrusion { height, base, .. } = layer else { panic!("3d-building is {}", layer.kind()) };
        let tags: std::collections::HashMap<String, TagValue> = [("height".to_string(), TagValue::Number(120.0))].into();
        assert_eq!(height.resolve(&tags, 16), 120.0);
        assert_eq!(base.resolve(&tags, 16), 0.0, "missing min_height: the case errors, base takes its default");
    }

    /// Task 9 review, Info 3: Standard's building fade is `15 -> 0, 15.3 ->
    /// alpha(colorBuildings)`. MapLibre reads it at the fractional frame
    /// zoom -- two thirds of the alpha at z15.2 (0.53 for an alpha of 0.8;
    /// this fixture's is 1, so 0.667) -- where the integer resample (`0` at
    /// 15, the alpha at 16) reads one fifth of it.
    #[test]
    fn standards_building_fade_reads_at_the_fractional_zoom() {
        let style = or_skip!(standard_fixture_with_3d_under_preset("night"));
        let layer = style.layers.iter().find(|l| l.id() == "3d-building").expect("3d-building compiles");
        let StyleLayer::Extrusion { opacity, .. } = layer else { panic!("3d-building is {}", layer.kind()) };
        let full = opacity.value_at_zoom(16.0);
        assert!(full > 0.0 && full <= 1.0, "alpha of colorBuildings: {full}");
        let at = opacity.value_at_zoom(15.2);
        assert!((at - full * 2.0 / 3.0).abs() < 1e-4, "z15.2 is two thirds up the 15 -> 15.3 ramp: {at} of {full}");
        assert_eq!(opacity.value_at_zoom(15.0), 0.0);
        assert_eq!(opacity.value_at_zoom(15.3), full);
        let plan = crate::render::frame_plan::extrusion_plan(
            &style.layers, 15.2, 0.0, 0.0,
            crate::render::frame_plan::ExtrusionOptions { translucency: true, ..Default::default() },
        );
        let p = plan.layers.iter().map(|l| l.params.opacity).fold(0.0_f32, f32::max);
        eprintln!("standard 3d-building opacity: z15.2 {at}, z16 {full}; planned {p}");
        assert!((p - at).abs() < 1e-4, "the frame plan draws it at {p}");
    }

    #[test]
    fn a_minzoom_10_extrusion_splits_at_zoom_12_into_a_flat_fill_below() {
        let style = parse_and_compile(br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[{"id":"b","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":10,
            "paint":{"fill-extrusion-color":"#888","fill-extrusion-height":20}}]}"##).expect("compiles");
        let ext = style.layers.iter().find(|l| matches!(l, StyleLayer::Extrusion { .. })).expect("extrusion");
        assert_eq!(ext.zoom_range(), (Some(12), None));
        let fill = style.layers.iter().find(|l| matches!(l, StyleLayer::Fill { .. })).expect("flat fallback");
        assert_eq!(fill.zoom_range(), (Some(10), Some(11)), "exclusive z12 boundary: the footprint's last bucket is 11");
        let StyleLayer::Extrusion { height, .. } = ext else { unreachable!() };
        assert_eq!(*height, crate::style::ExtrusionValue::Constant(20.0));
    }

    #[test]
    fn the_extrusion_floor_is_the_projection_blend_maximum() {
        // R1: extrusions draw only on flat frames, and "flat" is "zoom >= the blend maximum".
        assert_eq!(crate::style::EXTRUSION_MIN_ZOOM as f64, crate::render::globe::PROJECTION_BLEND_ZOOM_MAX);
    }

    #[test]
    fn a_non_numeric_height_resolves_to_zero() {
        use crate::mvt::TagValue;
        let get = crate::style::ExtrusionValue::Get("h".into());
        for value in [TagValue::String("12".into()), TagValue::Bool(true)] {
            let tags: std::collections::HashMap<String, TagValue> = [("h".to_string(), value)].into();
            assert_eq!(get.resolve(&tags, 16), 0.0);
        }
        let none: std::collections::HashMap<String, TagValue> = Default::default();
        assert_eq!(get.resolve(&none, 16), 0.0);
    }

    #[test]
    fn negative_and_nan_heights_clamp_like_maplibre() {
        use crate::mvt::TagValue;
        let get = crate::style::ExtrusionValue::Get("h".into());
        for (v, want) in [(-5.0, 0.0), (f64::NAN, 0.0), (f64::INFINITY, 0.0), (33.5, 33.5)] {
            let tags: std::collections::HashMap<String, TagValue> = [("h".to_string(), TagValue::Number(v))].into();
            assert_eq!(get.resolve(&tags, 16), want, "{v}");
        }
    }

    #[test]
    fn a_zoom_ramped_height_resolves_a_zoom_pair() {
        use crate::mvt::TagValue;
        let style = parse_and_compile(br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[{"id":"b","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":15,
            "paint":{"fill-extrusion-color":"#888",
            "fill-extrusion-height":["interpolate",["linear"],["zoom"],15,0,15.05,["get","height"]]}}]}"##).expect("compiles");
        let StyleLayer::Extrusion { height, .. } = &style.layers[0] else { panic!() };
        assert!(matches!(height, crate::style::ExtrusionValue::Feature { zoom_dependent: true, .. }));
        let tags: std::collections::HashMap<String, TagValue> = [("height".to_string(), TagValue::Number(100.0))].into();
        assert_eq!(height.resolve_pair(&tags, 15), (0.0, 100.0));
    }

    #[test]
    fn light_defaults_and_root_light_override() {
        let default = crate::style::ExtrusionLight::default();
        assert_eq!(default.anchor, crate::style::LightAnchor::Viewport);
        assert_eq!(default.position, [1.15, 210.0, 30.0]);
        assert_eq!(default.intensity, 0.5);
        let style = parse_and_compile(br##"{"version":8,"light":{"anchor":"map","position":[1.5,90,80],"color":"#ff0000","intensity":0.4},
            "sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[{"id":"b","type":"fill-extrusion","source":"s","source-layer":"building",
            "paint":{"fill-extrusion-color":"#888","fill-extrusion-height":20}}]}"##).expect("compiles");
        let StyleLayer::Extrusion { light, .. } = style.layers.iter().find(|l| matches!(l, StyleLayer::Extrusion { .. })).unwrap() else { unreachable!() };
        let light = light.at(16.0);
        assert_eq!(light.anchor, crate::style::LightAnchor::Map);
        assert_eq!(light.position, [1.5, 90.0, 80.0]);
        assert_eq!(light.intensity, 0.4);
        assert_eq!((light.color.r, light.color.g, light.color.b), (1.0, 0.0, 0.0));
    }

    #[test]
    fn standards_directional_light_maps_onto_the_maplibre_light() {
        let style = or_skip!(standard_fixture_with_3d_under_preset("day"));
        let StyleLayer::Extrusion { light, .. } = style.layers.iter().find(|l| l.id() == "3d-building").unwrap() else { panic!() };
        // R16: direction [az, polar] -> position [1.15, az, polar]; intensity carries over.
        assert_eq!(light.at(16.0).position[0], 1.15);
        assert_ne!(light.at(16.0), crate::style::ExtrusionLight::default(), "Standard declares a directional light");
    }

    /// Standard's `directional.intensity` is a `["zoom"]` interpolate over
    /// stops at z12, z13 and z14, and the buildings draw from z15, so a
    /// light folded once at z12 would shade them with the wrong value --
    /// the table must carry the value at the zoom drawn. The expected
    /// values are read off the fixture's expression: from z14, `night` is
    /// 0.5 (0.25 under the `monochrome` theme the fixture ships with) and
    /// `dusk` 0.2 (0.15 monochrome); at z12 `night` is 0 (0.01) and `dusk` 0.
    #[test]
    fn standards_night_and_dusk_buildings_get_their_z15_directional_intensity() {
        for (preset, theme, want, want_z12) in [
            ("night", Some("default"), 0.5, 0.0),
            ("dusk", Some("default"), 0.2, 0.0),
            ("night", None, 0.25, 0.01),
            ("dusk", None, 0.15, 0.0),
        ] {
            let style = or_skip!(standard_fixture_with_3d_under(preset, theme));
            let StyleLayer::Extrusion { light, min_zoom, .. } =
                style.layers.iter().find(|l| l.id() == "3d-building").unwrap()
            else {
                panic!()
            };
            assert_eq!(*min_zoom, Some(15));
            assert!((light.at(15.0).intensity - want).abs() < 1e-6, "{preset}/{theme:?} z15: {:?}", light.at(15.0));
            assert!((light.at(15.5).intensity - want).abs() < 1e-6, "{preset}/{theme:?} z15.5");
            assert!((light.at(12.0).intensity - want_z12).abs() < 1e-6, "{preset}/{theme:?} z12: the table is not one value");
        }
    }

    /// The boundary rule at `EXTRUSION_MIN_ZOOM` (see the
    /// `"fill-extrusion"` arm): MapLibre's maxzoom is exclusive, so an
    /// extrusion exists only for `maxzoom > 12`; below that the layer is
    /// its flat footprint alone, and it takes no group.
    #[test]
    fn a_maxzoom_at_or_below_the_floor_emits_no_extrusion_and_no_group() {
        let compile = |max: u8| {
            let json = format!(
                r##"{{"version":8,"sources":{{"s":{{"type":"vector","tiles":["x/{{z}}/{{x}}/{{y}}"]}}}},
                "layers":[
                {{"id":"low","type":"fill-extrusion","source":"s","source-layer":"building","maxzoom":{max},
                 "paint":{{"fill-extrusion-color":"#888","fill-extrusion-height":20}}}},
                {{"id":"next","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":14,
                 "paint":{{"fill-extrusion-color":"#888","fill-extrusion-height":20}}}}]}}"##
            );
            parse_and_compile(json.as_bytes()).expect("compiles")
        };
        let ranges = |style: &Style, id: &str| -> Vec<(&'static str, (Option<u8>, Option<u8>))> {
            style.layers.iter().filter(|l| l.id() == id).map(|l| (l.kind(), l.zoom_range())).collect()
        };
        let group_of_next = |style: &Style| {
            style.layers.iter().find_map(|l| match l {
                StyleLayer::Extrusion { id, group, .. } if id == "next" => Some(*group),
                _ => None,
            })
        };

        let z11 = compile(11);
        assert_eq!(ranges(&z11, "low"), vec![("fill", (None, Some(11)))]);
        assert_eq!(group_of_next(&z11), Some(0), "a layer with no extrusion takes no group");

        let z12 = compile(12);
        assert_eq!(ranges(&z12, "low"), vec![("fill", (None, Some(11)))], "MapLibre draws nothing extruded at z12");
        assert_eq!(group_of_next(&z12), Some(0));

        let z13 = compile(13);
        assert_eq!(ranges(&z13, "low"), vec![("fill-extrusion", (Some(12), Some(13))), ("fill", (None, Some(11)))]);
        assert_eq!(group_of_next(&z13), Some(1));
    }

    #[test]
    fn an_extrusion_group_is_shared_by_its_colour_cases() {
        let style = parse_and_compile(br##"{"version":8,"sources":{"s":{"type":"vector","tiles":["x/{z}/{x}/{y}"]}},
            "layers":[
            {"id":"a","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":14,
             "paint":{"fill-extrusion-color":["match",["get","k"],"x","#f00","#0f0"],"fill-extrusion-height":20}},
            {"id":"b","type":"fill-extrusion","source":"s","source-layer":"building","minzoom":14,
             "paint":{"fill-extrusion-color":"#00f","fill-extrusion-height":20}}]}"##).expect("compiles");
        let groups: Vec<(String, u16)> = style.layers.iter().filter_map(|l| match l {
            StyleLayer::Extrusion { id, group, .. } => Some((id.clone(), *group)), _ => None }).collect();
        assert!(groups.len() >= 3, "match colour splits into cases: {groups:?}");
        assert!(groups.iter().filter(|(id, _)| id == "a").all(|(_, g)| *g == 0));
        assert!(groups.iter().filter(|(id, _)| id == "b").all(|(_, g)| *g == 1));
    }

    #[test]
    fn a_literal_extrusion_translate_compiles_and_an_expression_degrades_to_zero() {
        let doc = |t: &str| format!(r##"{{"version":8,"sources":{{"s":{{"type":"vector","tiles":["x/{{z}}/{{x}}/{{y}}"]}}}},
            "layers":[{{"id":"b","type":"fill-extrusion","source":"s","source-layer":"b",
            "paint":{{"fill-extrusion-color":"#888","fill-extrusion-translate":{t},"fill-extrusion-translate-anchor":"viewport"}}}}]}}"##);
        let style = parse_and_compile(doc("[4, -2]").as_bytes()).unwrap();
        let StyleLayer::Extrusion { translate, translate_anchor, .. } = &style.layers[0] else { panic!() };
        assert_eq!(*translate, [4.0, -2.0]);
        assert_eq!(*translate_anchor, crate::style::TranslateAnchor::Viewport);
        let style = parse_and_compile(doc(r#"["interpolate",["linear"],["zoom"],15,["literal",[0,0]],16,["literal",[4,4]]]"#).as_bytes()).unwrap();
        let StyleLayer::Extrusion { translate, .. } = &style.layers[0] else { panic!() };
        assert_eq!(*translate, [0.0, 0.0]);
        // `["literal", [x, y]]` is a literal too; a missing anchor is `map`.
        let style = parse_and_compile(doc(r#"["literal", [1, 2]]"#).replace(r#","fill-extrusion-translate-anchor":"viewport""#, "").as_bytes()).unwrap();
        let StyleLayer::Extrusion { translate, translate_anchor, .. } = &style.layers[0] else { panic!() };
        assert_eq!((*translate, *translate_anchor), ([1.0, 2.0], crate::style::TranslateAnchor::Map));
    }

    /// Keeps `FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED` honest against
    /// the vendored styles rather than letting it rot into a comment. Every
    /// `fill-extrusion-*` property any vendored style actually writes must
    /// be either one this arm READS, or one that list names as consciously
    /// dropped. A newly vendored style that introduces a third category
    /// fails here, which is the moment to decide whether it matters -- not
    /// six months later when a building looks wrong.
    #[test]
    fn every_vendored_fill_extrusion_property_is_either_read_or_listed_as_ignored() {
        const READ: &[&str] = &[
            "fill-extrusion-color",
            "fill-extrusion-opacity",
            "fill-extrusion-emissive-strength",
            "fill-extrusion-color-use-theme",
            "fill-extrusion-height",
            "fill-extrusion-base",
            "fill-extrusion-vertical-gradient",
            "fill-extrusion-translate",
            "fill-extrusion-translate-anchor",
        ];

        let mut seen: Vec<String> = Vec::new();
        let mut layers = 0usize;
        // The count below spans all five, so the test needs every one.
        for bytes in [
            std::fs::read("examples/liberty-style.json").expect("run from the repo root"),
            mapbox_fixture_or_skip!("mapbox-streets-style.json"),
            mapbox_fixture_or_skip!("mapbox-standard-dark-style.json"),
            mapbox_fixture_or_skip!("swat-biz-light-style.json"),
            mapbox_fixture_or_skip!("swat-routes-dark-style.json"),
        ] {
            let root: Value = serde_json::from_slice(&bytes).unwrap();
            let mut documents = vec![&root];
            let imported = parse_imports(&root).unwrap();
            documents.extend(imported.iter().map(|import| &import.data));
            for document in documents {
                let Some(declared) = document.get("layers").and_then(Value::as_array) else { continue };
                for layer in declared {
                    if layer.get("type").and_then(Value::as_str) != Some("fill-extrusion") {
                        continue;
                    }
                    layers += 1;
                    let paint = layer.get("paint").and_then(Value::as_object);
                    for key in paint.into_iter().flat_map(|paint| paint.keys()) {
                        seen.push(key.clone());
                    }
                    let layout = layer.get("layout").and_then(Value::as_object);
                    for key in layout.into_iter().flat_map(|layout| layout.keys()) {
                        // `visibility` is every layer type's, handled well
                        // before the type dispatch, and not a
                        // fill-extrusion property at all.
                        if key != "visibility" {
                            seen.push(format!("layout.{key}"));
                        }
                    }
                }
            }
        }

        assert_eq!(layers, 7, "the vendored fill-extrusion layer count moved; revisit the audit");
        seen.sort();
        seen.dedup();
        let unaccounted: Vec<&String> = seen
            .iter()
            .filter(|key| {
                !READ.contains(&key.as_str()) && !FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED.contains(&key.as_str())
            })
            .collect();
        assert!(
            unaccounted.is_empty(),
            "a vendored style writes fill-extrusion properties this arm neither reads nor \
             consciously ignores: {unaccounted:?} -- decide which they are, then add them to \
             FILL_EXTRUSION_PROPERTIES_DELIBERATELY_IGNORED or to the arm"
        );
    }

    /// Every `StyleLayer` variant's `id`, in compiled order. Lets the
    /// `layout.visibility` tests below assert WHICH layers survived, not
    /// just how many -- a count alone would pass even if the wrong layer
    /// were dropped.
    fn layer_ids(style: &Style) -> Vec<&str> {
        style
            .layers
            .iter()
            // `StyleLayer::id()`, rather than a second copy of its match.
            // The copy was exhaustive over four variants, so a fifth broke
            // it -- work this helper never wanted to do, and a compile
            // error it never wanted to be the cause of. The method has
            // been the single place that answer lives all along.
            .map(StyleLayer::id)
            .collect()
    }

    /// A three-layer synthetic style whose MIDDLE layer is hidden. Every
    /// `layout.visibility` test here has to be synthetic: neither vendored
    /// style contains a single `"visibility": "none"` layer (0 of Liberty's
    /// 111, 0 of Mapbox Streets' 134), so a test written against those files
    /// would pass whether or not this property is honored at all. Mapbox's
    /// own Light and Dark styles are the real-world case -- they use it
    /// heavily (38 and 48 hidden layers respectively) to keep several layer
    /// variants in one document and switch between them.
    const HIDDEN_MIDDLE_LAYER_STYLE: &[u8] = br##"{"layers": [
        {"id": "first", "type": "fill", "source-layer": "water",
         "paint": {"fill-color": "#00f"}},
        {"id": "middle", "type": "fill", "source-layer": "water",
         "layout": {"visibility": "none"}, "paint": {"fill-color": "#f00"}},
        {"id": "third", "type": "fill", "source-layer": "landuse",
         "paint": {"fill-color": "#0f0"}}
    ]}"##;

    #[test]
    fn drops_a_layer_whose_layout_visibility_is_none() {
        let style = parse_and_compile(HIDDEN_MIDDLE_LAYER_STYLE).unwrap();
        assert_eq!(layer_ids(&style), vec!["first", "third"]);
    }

    #[test]
    fn treats_an_explicit_visible_and_an_absent_visibility_as_visible() {
        // `"visible"` and absence are the same thing in the style spec;
        // only `"none"` hides. A `layout` block that says nothing about
        // visibility (Liberty and Mapbox Streets are full of those --
        // that's where `text-field`/`icon-image` live) must not be read as
        // hidden either.
        let json = br##"{"layers": [
            {"id": "explicit-visible", "type": "fill", "source-layer": "water",
             "layout": {"visibility": "visible"}, "paint": {"fill-color": "#00f"}},
            {"id": "layout-without-visibility", "type": "fill", "source-layer": "water",
             "layout": {"fill-sort-key": 1}, "paint": {"fill-color": "#00f"}},
            {"id": "no-layout-at-all", "type": "fill", "source-layer": "water",
             "paint": {"fill-color": "#00f"}}
        ]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(
            layer_ids(&style),
            vec!["explicit-visible", "layout-without-visibility", "no-layout-at-all"]
        );
    }

    #[test]
    fn drops_a_hidden_layer_of_every_layer_type_not_just_one() {
        // The property is decided ONCE, in `parse_and_compile`, before the
        // layer type is dispatched at all -- so this asserts on all four
        // in-scope types plus an out-of-scope one. Mapbox Light/Dark hide
        // layers of several types, so a fix wired into a single `match` arm
        // would leave most of them drawing.
        let json = br##"{"layers": [
            {"id": "hidden-background", "type": "background",
             "layout": {"visibility": "none"}, "paint": {"background-color": "#fcfcfd"}},
            {"id": "visible-background", "type": "background",
             "paint": {"background-color": "#111"}},
            {"id": "hidden-fill", "type": "fill", "source-layer": "water",
             "layout": {"visibility": "none"}, "paint": {"fill-color": "#00f"}},
            {"id": "visible-fill", "type": "fill", "source-layer": "water",
             "paint": {"fill-color": "#00f"}},
            {"id": "hidden-line", "type": "line", "source-layer": "transportation",
             "layout": {"visibility": "none"}, "paint": {"line-color": "#888"}},
            {"id": "visible-line", "type": "line", "source-layer": "transportation",
             "paint": {"line-color": "#888"}},
            {"id": "hidden-symbol", "type": "symbol", "source-layer": "place",
             "layout": {"visibility": "none", "icon-image": "dot"}},
            {"id": "visible-symbol", "type": "symbol", "source-layer": "place",
             "layout": {"icon-image": "dot"}},
            {"id": "hidden-raster", "type": "raster", "source": "ne2",
             "layout": {"visibility": "none"}}
        ]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(
            layer_ids(&style),
            vec!["visible-background", "visible-fill", "visible-line", "visible-symbol"]
        );
    }

    #[test]
    fn a_hidden_layer_can_never_fail_the_style_however_broken_it_is() {
        // A layer the author switched OFF contributes nothing, so nothing
        // in it is worth validating -- including the shapes that are a hard
        // error on a visible layer (`background` with no
        // `background-color`, `fill` with no `source-layer`). Deciding
        // visibility before any of that parsing is what keeps one hidden
        // layer from taking down a whole style that never wanted to draw
        // it.
        let json = br##"{"layers": [
            {"id": "hidden-and-broken", "type": "background", "layout": {"visibility": "none"}},
            {"id": "hidden-and-rootless", "type": "fill", "layout": {"visibility": "none"}},
            {"id": "water", "type": "fill", "source-layer": "water", "paint": {"fill-color": "#00f"}}
        ]}"##;
        let style = parse_and_compile(json).expect("a hidden layer must not fail the style");
        assert_eq!(layer_ids(&style), vec!["water"]);
    }

    /// Env var naming the half of `emits_no_stderr_notice_for_a_hidden_layer`
    /// a given process should run: unset in the parent, `"hidden"` or
    /// `"control"` in the child.
    const STDERR_PROBE_MODE: &str = "GPUI_MAP_STYLE_SPEC_STDERR_PROBE";

    #[test]
    fn emits_no_stderr_notice_for_a_hidden_layer() {
        // `visibility: none` is deliberately NOT one of this module's
        // per-layer skip notices (see `parse_and_compile`), and "no output"
        // is only observable from OUTSIDE the process: libtest swallows
        // `eprintln!` unless `--nocapture`, and there's no capture hook to
        // hang a plain assertion on. So this test re-runs ITSELF in a child
        // process with capture off and reads the child's real stderr --
        // twice, because an assertion that stderr is empty is worthless
        // unless the same harness can be shown catching a notice that IS
        // emitted.
        match std::env::var(STDERR_PROBE_MODE).as_deref() {
            Ok("hidden") => {
                parse_and_compile(HIDDEN_MIDDLE_LAYER_STYLE).expect("hidden-layer style compiles");
                return;
            }
            Ok("control") => {
                // `heatmap`, not `raster`. This arm's whole job is to
                // prove the harness CAN see a skip notice, so it must name
                // a type that still produces one. When raster became a
                // compiled type this arm fell silent, and the assertion
                // below -- "the control run must show this harness CAN see
                // a skip notice" -- would have started failing for the
                // right reason: a control that controls nothing.
                let unsupported = br##"{"layers": [{"id": "hills", "type": "heatmap", "source": "ne2"}]}"##;
                parse_and_compile(unsupported).expect("heatmap-only style compiles");
                return;
            }
            _ => {}
        }

        let child_stderr = |mode: &str| -> String {
            let exe = std::env::current_exe().expect("this test binary's own path");
            let output = std::process::Command::new(exe)
                .args(["--exact", "--nocapture", "style_spec::tests::emits_no_stderr_notice_for_a_hidden_layer"])
                .env(STDERR_PROBE_MODE, mode)
                .output()
                .expect("re-running this test binary");
            assert!(output.status.success(), "child run {mode:?} failed: {output:?}");
            String::from_utf8_lossy(&output.stderr).into_owned()
        };

        let control = child_stderr("control");
        assert!(
            control.contains("style_spec: skipping layer"),
            "the control run must show this harness CAN see a skip notice, got {control:?}"
        );

        let hidden = child_stderr("hidden");
        assert!(
            hidden.trim().is_empty(),
            "a layer the style author turned off is honored, not skipped -- it must \
             print nothing, but the child wrote {hidden:?}"
        );
    }

    /// Mapbox Standard's real shape: `layout.visibility` is not always a
    /// bare string, it can be a `["case", ["config", k], "visible",
    /// "none"]` expression gating a whole layer category on one config
    /// key. This is the acceptance case for the fix -- both directions of
    /// the SAME layer, driven by nothing but the config value, so a
    /// regression that goes back to reading `.as_str()` (which is `None`
    /// on an array, so "not `\"none\"`, so visible") fails the `false` half
    /// silently by drawing a layer the config turned off.
    fn place_labels_style(show_place_labels: bool) -> Vec<u8> {
        format!(
            r##"{{
                "version": 8,
                "imports": [{{
                    "id": "basemap",
                    "config": {{ "showPlaceLabels": {show_place_labels} }},
                    "data": {{
                        "version": 8,
                        "schema": {{ "showPlaceLabels": {{ "default": true }} }},
                        "layers": [
                            {{ "id": "settlement-major-label", "type": "symbol", "source-layer": "place",
                               "layout": {{ "visibility": ["case", ["config", "showPlaceLabels"], "visible", "none"],
                                            "icon-image": "dot" }} }}
                        ]
                    }}
                }}],
                "layers": []
            }}"##
        )
        .into_bytes()
    }

    #[test]
    fn hides_a_layer_whose_expression_valued_visibility_folds_to_none() {
        let style = parse_and_compile(&place_labels_style(false)).expect("must compile");
        assert_eq!(layer_ids(&style), Vec::<&str>::new(), "showPlaceLabels: false must hide the label layer");
    }

    #[test]
    fn draws_a_layer_whose_expression_valued_visibility_folds_to_visible() {
        let style = parse_and_compile(&place_labels_style(true)).expect("must compile");
        assert_eq!(layer_ids(&style), vec!["settlement-major-label"], "showPlaceLabels: true must draw the label layer");
    }

    /// The other side of the fix's own risk: an expression-valued
    /// `visibility` the folder genuinely cannot reduce (here, the config it
    /// reaches for is not resolved at all -- no `imports`, so `scope` in
    /// `parse_and_compile`'s top-level loop carries no config) must not be
    /// read as "not none, so visible" -- the exact bug this fix closes,
    /// just for a different unfoldable shape. It hides, loudly.
    #[test]
    fn hides_a_layer_whose_visibility_expression_cannot_fold_at_all() {
        let json = br##"{"layers": [
            {"id": "unresolvable", "type": "fill", "source-layer": "water",
             "layout": {"visibility": ["case", ["config", "showPlaceLabels"], "visible", "none"]},
             "paint": {"fill-color": "#00f"}}
        ]}"##;
        let style = parse_and_compile(json).expect("an unfoldable visibility must not fail the whole style");
        assert_eq!(layer_ids(&style), Vec::<&str>::new(), "an unfoldable visibility must hide, not draw");
    }

    #[test]
    fn applies_a_real_line_opacity_shape_to_the_resolved_alpha_multiplicatively() {
        // `admin-1-boundary-bg`'s real line-opacity: fades in between z7 and
        // z8. A distinctive base alpha (0.8, via hsla) confirms the final
        // alpha is the color's own alpha TIMES the opacity, not just the
        // opacity value replacing it.
        let json = br##"{"layers": [{
            "id": "admin-1-boundary-bg", "type": "line", "source-layer": "admin",
            "paint": {
                "line-color": "hsla(0, 0%, 0%, 0.8)",
                "line-opacity": ["interpolate", ["linear"], ["zoom"], 7, 0, 8, 0.5]
            }
        }]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(style.layers.len(), 1, "line-opacity isn't match-shaped here, so no cross-product expansion");

        let tile = tile_with_feature("admin", HashMap::new(), line_geometry());

        let plan_at_seven = match_tile(&tile, &style, 7);
        let alpha_seven = match &plan_at_seven.ops[..] {
            [PaintOp::Line { color_from, .. }] => color_from.a,
            other => panic!("expected exactly one Line op, got {other:?}"),
        };
        assert!(alpha_seven < 0.01, "expected near-zero alpha at zoom 7, got {alpha_seven}");

        let plan_at_eight = match_tile(&tile, &style, 8);
        let alpha_eight = match &plan_at_eight.ops[..] {
            [PaintOp::Line { color_from, .. }] => color_from.a,
            other => panic!("expected exactly one Line op, got {other:?}"),
        };
        assert!(
            (alpha_eight - 0.4).abs() < 1e-3,
            "expected 0.8 (color alpha) * 0.5 (opacity) = 0.4, got {alpha_eight}"
        );
    }

    #[test]
    fn cross_products_landcovers_real_fill_opacity_with_its_real_fill_color() {
        // `landcover`'s real fill-color and fill-opacity: fill-opacity is
        // itself a `match` nested inside an `interpolate`, so this proves
        // the opacity cross-product picks the right case per feature rather
        // than always falling back to one branch.
        let json = br##"{"layers": [{
            "id": "landcover", "type": "fill", "source-layer": "landcover",
            "paint": {
                "fill-color": ["match", ["get", "class"], "wood", "hsla(115, 55%, 74%, 0.8)", "snow", "hsl(200, 70%, 90%)", "hsl(110, 52%, 81%)"],
                "fill-opacity": ["interpolate", ["exponential", 1.5], ["zoom"], 9, 0.4, 10, ["match", ["get", "class"], "crop", 0, 0.4], 11, ["match", ["get", "class"], "crop", 0, 0.4], 12, 0]
            }
        }]}"##;
        let style = parse_and_compile(json).unwrap();

        let alpha_at = |class: &str, zoom: u8| {
            let tile = tile_with_feature("landcover", tags(&[("class", class)]), polygon_geometry());
            let plan = match_tile(&tile, &style, zoom);
            match &plan.ops[..] {
                [PaintOp::Fill { color_from, .. }] => color_from.a,
                other => panic!("expected exactly one Fill op for class {class:?} at zoom {zoom}, got {other:?}"),
            }
        };

        // "crop" has its own explicit fill-opacity case (0 at zoom 10):
        // the opacity cross-product must pick it, not the default 0.4.
        let crop_alpha = alpha_at("crop", 10);
        assert!(crop_alpha < 1e-3, "expected crop's alpha to be ~0 at zoom 10, got {crop_alpha}");

        // "wood" has its own fill-color case (alpha 0.8) but isn't an
        // explicit fill-opacity case, so it gets the opacity match's
        // DEFAULT case (0.4) -- final alpha is 0.8 * 0.4, not just 0.4.
        let wood_alpha = alpha_at("wood", 10);
        assert!(
            (wood_alpha - 0.8 * 0.4).abs() < 1e-3,
            "expected wood's alpha to be 0.8 (its color's alpha) * 0.4 (default opacity case) = 0.32, got {wood_alpha}"
        );
    }

    #[test]
    fn compiles_a_literal_text_field() {
        let json = serde_json::json!("Hello");
        let field = compile_text_field(&json).unwrap();
        assert!(matches!(field, crate::style::TextField::Literal(s) if s == "Hello"));
    }

    #[test]
    fn compiles_a_get_text_field() {
        let json = serde_json::json!(["get", "name"]);
        let field = compile_text_field(&json).unwrap();
        assert!(matches!(field, crate::style::TextField::Get(k) if k == "name"));
    }

    #[test]
    fn compiles_the_real_coalesce_name_en_name_shape() {
        let json = serde_json::json!(["coalesce", ["get", "name_en"], ["get", "name"]]);
        let field = compile_text_field(&json).unwrap();
        assert!(matches!(&field, crate::style::TextField::Coalesce(keys) if keys == &vec!["name_en".to_string(), "name".to_string()]));
    }

    /// The exact `text-field` every OpenMapTiles label layer carries, copied
    /// verbatim from the vendored Liberty style. One unsupported shape was
    /// taking 20 of the 25 symbol layers with it -- every place name, street
    /// name, POI, water name and airport.
    #[test]
    fn compiles_the_real_bilingual_text_field() {
        use crate::style::{ConcatPart, TextField};
        let json = serde_json::json!([
            "case",
            ["has", "name:nonlatin"],
            ["concat", ["get", "name:latin"], " ", ["get", "name:nonlatin"]],
            ["coalesce", ["get", "name_en"], ["get", "name"]]
        ]);
        let field = compile_text_field(&json).expect("the real label shape must compile");
        assert_eq!(
            field,
            TextField::CaseHas {
                key: "name:nonlatin".to_string(),
                then: Box::new(TextField::Concat(vec![
                    ConcatPart::Get("name:latin".to_string()),
                    ConcatPart::Literal(" ".to_string()),
                    ConcatPart::Get("name:nonlatin".to_string()),
                ])),
                otherwise: Box::new(TextField::Coalesce(vec![
                    "name_en".to_string(),
                    "name".to_string()
                ])),
            }
        );
    }

    /// The road shields' own shape. `ref` arrives as a NUMBER in real tiles,
    /// which is the whole reason the style wraps it in `to-string`.
    #[test]
    fn compiles_the_real_shield_text_field() {
        use crate::style::TextField;
        let json = serde_json::json!(["to-string", ["get", "ref"]]);
        assert_eq!(
            compile_text_field(&json).expect("the shield shape must compile"),
            TextField::ToStringGet("ref".to_string())
        );
    }

    /// A `case` with more arms than the single-condition form is still out of
    /// scope, and must skip the layer rather than compile to something that
    /// silently drops an arm.
    #[test]
    fn rejects_a_multi_arm_text_field_case() {
        let json = serde_json::json!([
            "case",
            ["has", "a"], ["get", "a"],
            ["has", "b"], ["get", "b"],
            ["get", "c"]
        ]);
        assert!(compile_text_field(&json).is_err(), "a two-arm case must not compile");
    }

    #[test]
    fn rejects_an_unsupported_text_field_shape() {
        // `concat` and `case` now compile; `upcase` is still out of scope,
        // and the point of this test is that an unknown shape is an `Err` the
        // caller skips on, never a panic.
        let json = serde_json::json!(["upcase", ["get", "name"]]);
        assert!(compile_text_field(&json).is_err());
    }

    #[test]
    fn compiles_a_literal_icon_image() {
        let json = serde_json::json!("marker");
        let icon = compile_icon_image(&json).unwrap();
        assert!(matches!(icon, crate::style::IconImage::Literal(s) if s == "marker"));
    }

    #[test]
    fn compiles_a_get_icon_image() {
        let json = serde_json::json!(["get", "network"]);
        let icon = compile_icon_image(&json).unwrap();
        assert!(matches!(icon, crate::style::IconImage::Get(k) if k == "network"));
    }

    #[test]
    fn compiles_a_whole_expression_to_string_icon_image() {
        // Liberty's `poi_transit` shape. `compile_concat_parts` already
        // accepted `["to-string", ["get", k]]` as one PART of a concat; this
        // is the same expression standing alone as the whole `icon-image`.
        let json = serde_json::json!(["to-string", ["get", "class"]]);
        let icon = compile_icon_image(&json).unwrap();
        assert!(matches!(&icon, crate::style::IconImage::ToStringGet(k) if k == "class"));

        let mut tags = HashMap::new();
        tags.insert("class".to_string(), TagValue::String("bus".to_string()));
        assert_eq!(icon.resolve(&tags, 14).as_deref(), Some("bus"));
    }

    #[test]
    fn compiles_the_real_maki_beta_coalesce_shape() {
        let json = serde_json::json!(["coalesce", ["image", ["get", "maki_beta"]], ["image", ["get", "maki"]]]);
        let icon = compile_icon_image(&json).unwrap();
        assert!(matches!(&icon, crate::style::IconImage::CoalesceImage(keys) if keys == &vec!["maki_beta".to_string(), "maki".to_string()]));
    }

    #[test]
    fn compiles_the_real_poi_label_case_wrapped_coalesce_shape() {
        // The real Mapbox Streets v12 poi-label icon-image: a `case` guard
        // wrapped around the maki_beta/maki coalesce above. The guard is
        // redundant (see compile_icon_image's doc comment) and should
        // unwrap to the identical `CoalesceImage(["maki_beta", "maki"])`.
        let json = serde_json::json!([
            "case",
            ["has", "maki_beta"],
            ["coalesce", ["image", ["get", "maki_beta"]], ["image", ["get", "maki"]]],
            ["image", ["get", "maki"]]
        ]);
        let icon = compile_icon_image(&json).unwrap();
        assert!(matches!(&icon, crate::style::IconImage::CoalesceImage(keys) if keys == &vec!["maki_beta".to_string(), "maki".to_string()]));

        let mut has_beta = HashMap::new();
        has_beta.insert("maki_beta".to_string(), TagValue::String("restaurant-beta".to_string()));
        has_beta.insert("maki".to_string(), TagValue::String("restaurant".to_string()));
        assert_eq!(icon.resolve(&has_beta, 14), Some("restaurant-beta".to_string()));

        let mut maki_only = HashMap::new();
        maki_only.insert("maki".to_string(), TagValue::String("restaurant".to_string()));
        assert_eq!(icon.resolve(&maki_only, 14), Some("restaurant".to_string()));

        let neither = HashMap::new();
        assert_eq!(icon.resolve(&neither, 14), None);
    }

    #[test]
    fn rejects_a_case_wrapped_icon_image_whose_keys_do_not_line_up() {
        // The else-branch key ("z") doesn't match the coalesce's fallback
        // key ("y"), so this is NOT the redundant poi-label pattern -- it's
        // a genuinely different (and unsupported) case expression. Must be
        // an `Err`, not silently treated as the redundant shape.
        let json = serde_json::json!([
            "case",
            ["has", "x"],
            ["coalesce", ["image", ["get", "x"]], ["image", ["get", "y"]]],
            ["image", ["get", "z"]]
        ]);
        assert!(compile_icon_image(&json).is_err());
    }

    #[test]
    fn compiles_a_zoom_step_icon_image() {
        let json = serde_json::json!(["step", ["zoom"], "oneway-small", 18, "oneway-large"]);
        let icon = compile_icon_image(&json).unwrap();
        assert!(matches!(icon, crate::style::IconImage::ZoomStep(_)));
    }

    #[test]
    fn compiles_a_concat_icon_image() {
        let json = serde_json::json!(
            ["concat", ["get", "shield"], "-", ["to-string", ["get", "reflen"]]]
        );
        let icon = compile_icon_image(&json).expect("concat icon-image must compile");
        let mut tags = std::collections::HashMap::new();
        tags.insert("shield".to_string(), crate::mvt::TagValue::String("us-interstate".to_string()));
        tags.insert("reflen".to_string(), crate::mvt::TagValue::Number(2.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("us-interstate-2"));
    }

    #[test]
    fn compiles_the_motorway_exit_concat_icon_image() {
        let json = serde_json::json!(["concat", "motorway-exit-", ["to-string", ["get", "reflen"]]]);
        let icon = compile_icon_image(&json).expect("must compile");
        let mut tags = std::collections::HashMap::new();
        tags.insert("reflen".to_string(), crate::mvt::TagValue::Number(4.0));
        assert_eq!(icon.resolve(&tags, 14).as_deref(), Some("motorway-exit-4"));
    }

    #[test]
    fn a_concat_icon_image_with_an_unsupported_part_is_still_an_error() {
        // Anything beyond a literal, ["get", k] and ["to-string", ["get", k]]
        // stays out of scope, so the caller's text-only degradation applies.
        let json = serde_json::json!(["concat", ["upcase", ["get", "shield"]], "-", "3"]);
        assert!(compile_icon_image(&json).is_err());
    }

    #[test]
    fn compiles_the_real_road_number_shield_icon_image() {
        // The exact shape from examples/mapbox-streets-style.json.
        let json = serde_json::json!(["case",
            ["has", "shield_beta"],
            ["coalesce",
                ["image", ["concat", ["get", "shield_beta"], "-", ["to-string", ["get", "reflen"]]]],
                ["image", ["concat", "default-", ["to-string", ["get", "reflen"]]]]],
            ["concat", ["get", "shield"], "-", ["to-string", ["get", "reflen"]]]]);
        let icon = compile_icon_image(&json).expect("the real shield icon-image must compile");

        let mut tags = std::collections::HashMap::new();
        tags.insert("shield".to_string(), crate::mvt::TagValue::String("rectangle-blue".to_string()));
        tags.insert("reflen".to_string(), crate::mvt::TagValue::Number(3.0));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("rectangle-blue-3"));

        // With the beta key present, the then-branch's FIRST coalesce entry
        // wins: resolve() cannot see the atlas, so it cannot fall back on a
        // missing sprite. Recorded divergence, unreachable on today's data.
        tags.insert("shield_beta".to_string(), crate::mvt::TagValue::String("rectangle-green".to_string()));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("rectangle-green-3"));
    }

    #[test]
    fn the_poi_label_case_wrapped_coalesce_still_compiles_to_coalesce_image() {
        // Guards the pre-existing arm this task extends: poi-label's redundant
        // guard must still unwrap to CoalesceImage, not become a CaseHas.
        let json = serde_json::json!(["case",
            ["has", "maki_beta"],
            ["coalesce", ["image", ["get", "maki_beta"]], ["image", ["get", "maki"]]],
            ["image", ["get", "maki"]]]);
        let icon = compile_icon_image(&json).expect("must compile");
        assert!(matches!(icon, crate::style::IconImage::CoalesceImage(_)));
    }

    #[test]
    fn compiles_a_zoom_stepped_icon_image() {
        let json = serde_json::json!(["step", ["zoom"], "oneway-small", 18, "oneway-large"]);
        let icon = compile_icon_image(&json).expect("stepped icon-image must compile");
        let tags = std::collections::HashMap::new();
        assert_eq!(icon.resolve(&tags, 17).as_deref(), Some("oneway-small"));
        assert_eq!(icon.resolve(&tags, 18).as_deref(), Some("oneway-large"), "the stop is inclusive");
        assert_eq!(icon.resolve(&tags, 19).as_deref(), Some("oneway-large"));
    }

    #[test]
    fn compiles_a_multi_stop_zoom_stepped_icon_image() {
        let json = serde_json::json!(["step", ["zoom"], "a", 12, "b", 16, "c"]);
        let icon = compile_icon_image(&json).expect("must compile");
        let tags = std::collections::HashMap::new();
        assert_eq!(icon.resolve(&tags, 11).as_deref(), Some("a"));
        assert_eq!(icon.resolve(&tags, 12).as_deref(), Some("b"));
        assert_eq!(icon.resolve(&tags, 15).as_deref(), Some("b"));
        assert_eq!(icon.resolve(&tags, 16).as_deref(), Some("c"));
    }

    /// `["image", name, {"params": {...}}]` -- Mapbox's runtime-recolourable
    /// image expression (Mapbox GL JS 3.15.0
    /// `style-spec/expression/definitions/image.ts`), the shape all eleven of
    /// Mapbox Standard's one-way-arrow and crosswalk layers name their sprite
    /// with. The NAME is what this crate can use; the `params` are dropped,
    /// loudly.
    #[test]
    fn compiles_an_image_expression_and_records_its_dropped_params_as_a_degradation() {
        let _ = take_degradations();
        let json = serde_json::json!(["image", "oneway-small", {"params": {"color": "hsl(0, 0%, 100%)"}}]);
        let icon = compile_icon_image(&json).expect("an [\"image\", name, options] icon-image must compile");
        assert_eq!(icon.resolve(&HashMap::new(), 16).as_deref(), Some("oneway-small"));

        let degraded = take_degradations();
        assert_eq!(degraded.len(), 1, "the dropped params must be recorded once: {degraded:?}");
        assert!(
            degraded[0].contains("params"),
            "the notice must name what was dropped, got: {}",
            degraded[0]
        );
    }

    /// The same expression WITHOUT options is not a degradation at all --
    /// there is nothing to drop, so nothing may be recorded.
    #[test]
    fn a_bare_image_expression_compiles_with_no_degradation() {
        let _ = take_degradations();
        let icon = compile_icon_image(&serde_json::json!(["image", "oneway-large"])).expect("must compile");
        assert_eq!(icon.resolve(&HashMap::new(), 16).as_deref(), Some("oneway-large"));
        assert!(take_degradations().is_empty(), "a bare [\"image\", name] drops nothing");
    }

    /// `road-oneway-arrow`'s real `icon-image`, VERBATIM from
    /// `examples/mapbox-standard-dark-style.json`: a `let` whose binding is
    /// referenced only from inside the `params` this crate drops, wrapped
    /// around a zoom `step` between two `["image", ...]` sprites. The arrows
    /// are the whole point -- z16-17 draws the small arrow, z18+ the large
    /// one.
    #[test]
    fn compiles_the_real_one_way_arrow_icon_image_through_its_let_and_image_params() {
        let json = serde_json::json!(["let", "l_colorRoads", ["at", 2, ["to-hsla", ["config", "colorRoads"]]],
            ["step", ["zoom"],
                ["image", "oneway-small", {"params": {"color": ["case",
                    ["all", [">=", ["var", "l_colorRoads"], 92],
                        ["match", ["config", "lightPreset"], ["dusk", "night"], false, true]],
                    "hsl(0, 0%, 85%)", "hsl(0, 0%, 100%)"]}}],
                18,
                ["image", "oneway-large", {"params": {"color": "hsl(0, 0%, 100%)"}}]]]);
        let icon = compile_icon_image(&json).expect("the real one-way-arrow icon-image must compile");
        assert_eq!(icon.resolve(&HashMap::new(), 16).as_deref(), Some("oneway-small"));
        assert_eq!(icon.resolve(&HashMap::new(), 17).as_deref(), Some("oneway-small"));
        assert_eq!(icon.resolve(&HashMap::new(), 18).as_deref(), Some("oneway-large"));
        assert_eq!(icon.resolve(&HashMap::new(), 22).as_deref(), Some("oneway-large"));
    }

    /// `tunnel-oneway-arrow-blue`'s real `icon-image` (the root document's
    /// own overlay layer): a zoom `step` whose MIDDLE bracket is a `match`
    /// on the feature's `class`. The other ten arrow layers never needed a
    /// per-zoom `match`; this one does, and without it the layer skipped.
    #[test]
    fn compiles_a_per_zoom_match_icon_image() {
        let json = serde_json::json!(["step", ["zoom"], "oneway-small", 17,
            ["match", ["get", "class"], ["primary", "secondary", "tertiary", "street", "street_limited"],
                "oneway-large", "oneway-small"],
            18, "oneway-large"]);
        let icon = compile_icon_image(&json).expect("a per-zoom match icon-image must compile");

        // Below the first stop every class takes the base bracket.
        assert_eq!(icon.resolve(&tags(&[("class", "primary")]), 16).as_deref(), Some("oneway-small"));
        // In the z17 bracket the match decides, per class.
        assert_eq!(icon.resolve(&tags(&[("class", "primary")]), 17).as_deref(), Some("oneway-large"));
        assert_eq!(icon.resolve(&tags(&[("class", "service")]), 17).as_deref(), Some("oneway-small"));
        // A feature with no `class` at all equals no label -- the fallback.
        assert_eq!(icon.resolve(&HashMap::new(), 17).as_deref(), Some("oneway-small"));
        // From z18 both classes take the large arrow again.
        assert_eq!(icon.resolve(&tags(&[("class", "service")]), 18).as_deref(), Some("oneway-large"));
    }

    /// A zoom step whose brackets are `["concat", ...]` sprite names --
    /// `poi-label`'s real `icon-image`, which asks for the 11px maki icon
    /// below z17 and the 15px one above. Was refused outright until Mapbox
    /// Standard supplied a real layer that needs it.
    #[test]
    fn compiles_a_zoom_stepped_concat_icon_image() {
        let json = serde_json::json!(["step", ["zoom"],
            ["concat", ["get", "maki"], "-11"], 17, ["concat", ["get", "maki"], "-15"]]);
        let icon = compile_icon_image(&json).expect("a zoom step of concat sprite names must compile");
        let restaurant = tags(&[("maki", "restaurant")]);
        assert_eq!(icon.resolve(&restaurant, 16).as_deref(), Some("restaurant-11"));
        assert_eq!(icon.resolve(&restaurant, 17).as_deref(), Some("restaurant-15"));
        // A feature with no `maki` names no sprite at all -- never the
        // half-name "-11", which would miss in the atlas on every feature.
        assert_eq!(icon.resolve(&HashMap::new(), 17), None);
    }

    /// A `["step", ["zoom"], ...]` icon-image whose brackets are a shape
    /// with no faithful encoding is still an `Err`, so the caller's
    /// text-only degradation applies. `to-string` as a whole bracket is one:
    /// `DataIconShape` has a `Concat` case but no bare `ToStringGet`.
    #[test]
    fn a_stepped_icon_image_with_non_string_outputs_is_still_unsupported() {
        let json = serde_json::json!(["step", ["zoom"], ["to-string", ["get", "ref"]], 11, "x"]);
        assert!(compile_icon_image(&json).is_err());
    }

    /// `transit-stop-label`'s real `text-offset`, VERBATIM from
    /// `examples/mapbox-standard-dark-style.json`: a zoom `interpolate`
    /// whose stop outputs are `["literal", [x, y]]` pairs. This layer is
    /// Singapore's bus stops and MRT stations, and this one expression was
    /// the whole reason it never drew.
    #[test]
    fn compiles_a_zoom_interpolated_pair_layout_value() {
        let json =
            serde_json::json!(["interpolate", ["linear"], ["zoom"], 0, ["literal", [0, 0]], 17, ["literal", [0, 0.75]]]);
        let (x, y) = compile_layout_value_pair(&json).expect("a zoom-interpolated text-offset must compile");
        let no_tags = HashMap::new();

        // x is 0 across the whole curve; y ramps 0 -> 0.75 over z0..z17 and
        // clamps outside it, exactly as the style spec's `interpolate` does.
        for zoom in [0u8, 8, 17, 22] {
            assert_eq!(x.resolve(&no_tags, zoom), 0.0, "x at z{zoom}");
        }
        assert_eq!(y.resolve(&no_tags, 0), 0.0);
        assert!((y.resolve(&no_tags, 17) - 0.75).abs() < 1e-6, "{}", y.resolve(&no_tags, 17));
        assert!((y.resolve(&no_tags, 22) - 0.75).abs() < 1e-6, "must clamp above the last stop");
        let midpoint = y.resolve(&no_tags, 8);
        assert!(
            (midpoint - 0.75 * 8.0 / 17.0).abs() < 1e-4,
            "a linear ramp must be sampled linearly, got {midpoint}"
        );
    }

    /// A PROPERTY-driven curve is a different compiler from the zoom one --
    /// `compile_zoom_value` rejects a `["get", ...]` interpolation input and
    /// always will -- and lands in `LayoutValue::ZoomOfDataRamp` instead.
    ///
    /// This is half of `poi-label`'s real `text-offset`, and until it
    /// compiled the property degraded to no offset at all, which is what
    /// left every POI name sitting on its own icon.
    #[test]
    fn compiles_a_property_driven_interpolate_pair_layout_value_as_a_ramp() {
        let json = serde_json::json!(["interpolate", ["linear"], ["get", "sizerank"],
            0, ["literal", [0, 0]], 5, ["literal", [0, 0.75]]]);
        let (x, y) = compile_layout_value_pair(&json).expect("a property ramp must compile");
        let ranked = |rank: f64| HashMap::from([("sizerank".to_string(), TagValue::Number(rank))]);

        // x never moves; y ramps 0 -> 0.75 across sizerank 0..5 and clamps
        // outside it, and the zoom is irrelevant to all of it.
        for rank in [0.0, 2.5, 5.0, 40.0] {
            assert_eq!(x.resolve(&ranked(rank), 17), 0.0, "x at sizerank {rank}");
        }
        assert_eq!(y.resolve(&ranked(0.0), 17), 0.0);
        assert!((y.resolve(&ranked(2.5), 17) - 0.375).abs() < 1e-6, "{}", y.resolve(&ranked(2.5), 17));
        assert!((y.resolve(&ranked(5.0), 17) - 0.75).abs() < 1e-6);
        assert!((y.resolve(&ranked(40.0), 17) - 0.75).abs() < 1e-6, "must clamp above the last stop");
        // A feature with no `sizerank` takes the lowest stop, which for
        // every real text-offset ramp is the style-spec default of [0, 0].
        assert_eq!(y.resolve(&HashMap::new(), 17), 0.0);
    }

    /// A non-`linear` property curve is refused rather than approximated --
    /// see `DataRampShape`'s doc comment. The caller degrades the property,
    /// which is the same trade an unreadable `text-offset` has always made.
    #[test]
    fn a_property_driven_interpolate_with_a_curve_is_refused() {
        let json = serde_json::json!(["interpolate", ["exponential", 1.5], ["get", "sizerank"],
            0, ["literal", [0, 0]], 5, ["literal", [0, 0.75]]]);
        assert!(compile_layout_value_pair(&json).is_err());
    }

    /// Acceptance, on the real layer: `transit-stop-label` compiles end to
    /// end out of the vendored document and keeps its `["get", "maki"]`
    /// icon, which is the bus-stop/MRT sprite.
    #[test]
    fn the_real_transit_stop_label_layer_compiles_with_its_icon() {
        let layer = or_skip!(standard_dark_layer("transit-stop-label"));
        let compiled = compile_symbol_layer(&layer, Scope::root())
            .expect("transit-stop-label must no longer skip on its text-offset");
        assert_eq!(compiled.len(), 1);
        let StyleLayer::Symbol { icon_image, text_offset_y, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let bus_stop = tags(&[("maki", "bus")]);
        assert_eq!(
            icon_image.as_ref().expect("the layer names an icon").resolve(&bus_stop, 17).as_deref(),
            Some("bus")
        );
        assert!((text_offset_y.resolve(&bus_stop, 17) - 0.75).abs() < 1e-6);
    }

    /// A `text-offset` this crate cannot read must cost the layer its
    /// OFFSET, not its symbols -- the same trade `text-anchor` immediately
    /// below it already makes, and for the same reason: a label a little
    /// out of place is a far smaller defect than a layer with no labels and
    /// no icons at all.
    ///
    /// `poi-label`'s real ramp over `sizerank` USED to be the example here;
    /// it compiles now (`LayoutValue::ZoomOfDataRamp`), and the standing
    /// example is a property curve with a non-linear easing, which
    /// `DataRampShape` refuses rather than approximates.
    #[test]
    fn an_unsupported_text_offset_degrades_to_zero_instead_of_skipping_the_layer() {
        let _ = take_degradations();
        let layer = serde_json::json!({
            "id": "offset-we-cannot-read",
            "type": "symbol",
            "source-layer": "poi_label",
            "layout": {
                "text-field": ["get", "name"],
                "icon-image": ["get", "maki"],
                "text-offset": ["interpolate", ["cubic-bezier", 0.2, 0, 0.9, 1], ["get", "sizerank"],
                    0, ["literal", [0, 0]], 5, ["literal", [0, 0.75]]]
            }
        });
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("the layer must survive");
        assert_eq!(compiled.len(), 1);
        let StyleLayer::Symbol { text_offset_x, text_offset_y, icon_image, .. } = &compiled[0] else {
            panic!("expected a Symbol layer");
        };
        assert_eq!(text_offset_x.resolve(&HashMap::new(), 17), 0.0);
        assert_eq!(text_offset_y.resolve(&HashMap::new(), 17), 0.0);
        assert!(icon_image.is_some(), "the icon -- the point of keeping the layer -- must survive");

        let (capability, folder) = split_degradations(take_degradations());
        assert!(folder.is_empty(), "nothing here is a folder failure: {folder:?}");
        assert_eq!(capability.len(), 1, "the dropped offset must be recorded: {capability:?}");
        assert!(capability[0].contains("text-offset"), "{}", capability[0]);
    }

    /// A filter's `["let", name, value, ..., body]` inlines at its `var`
    /// sites and then compiles as an ordinary filter -- `compile_let`'s
    /// substitution primitive, on `compile_filter`'s side of the house.
    #[test]
    fn compiles_a_let_filter_by_inlining_it_at_its_var_sites() {
        let json = serde_json::json!(["let", "c", ["get", "class"],
            ["all", ["==", ["var", "c"], "park"], [">=", ["get", "sizerank"], 3]]]);
        let filter = compile_filter(&json, Scope::root()).expect("a let filter must compile");

        let mut park = tags(&[("class", "park")]);
        park.insert("sizerank".to_string(), TagValue::Number(4.0));
        assert!(filter.matches(&park, 14.0));

        let mut small_park = tags(&[("class", "park")]);
        small_park.insert("sizerank".to_string(), TagValue::Number(1.0));
        assert!(!filter.matches(&small_park, 14.0));

        let mut school = tags(&[("class", "school")]);
        school.insert("sizerank".to_string(), TagValue::Number(4.0));
        assert!(!filter.matches(&school, 14.0));
    }

    /// `poi-label`'s real filter, VERBATIM from the vendored document: a
    /// `let` whose binding is a per-class density number, used inside a
    /// `case` on the right-hand side of a `<=`. Inlining is exact, but the
    /// resulting comparison -- a feature-driven threshold that also varies
    /// with zoom -- has no `Filter` encoding, so it degrades to "match
    /// everything" and KEEPS the layer. Dropping the layer instead would
    /// cost every POI icon on the map, which is what this whole change is
    /// for.
    #[test]
    fn a_let_filter_this_crate_cannot_represent_degrades_to_any_instead_of_failing() {
        let _ = take_degradations();
        let filter = compile_filter(&or_skip!(standard_dark_layer("poi-label"))["filter"], Scope::root())
            .expect("an unrepresentable let filter must not fail the layer");
        assert!(matches!(filter, Filter::Any), "{filter:?}");
        assert!(filter.matches(&tags(&[("class", "food_and_drink")]), 17.0));

        let (capability, folder) = split_degradations(take_degradations());
        assert!(folder.is_empty(), "nothing here is a folder failure: {folder:?}");
        assert_eq!(capability.len(), 1, "the dropped filter must be recorded: {capability:?}");
        assert!(capability[0].contains("let"), "{}", capability[0]);
    }

    /// A `let` filter whose body is structurally BROKEN is still an error,
    /// not a degradation: the degradation exists for a well-formed filter
    /// this crate cannot represent, never as a way to swallow malformed
    /// input.
    #[test]
    fn a_malformed_let_filter_is_still_an_error() {
        assert!(compile_filter(&serde_json::json!(["let", "c"]), Scope::root()).is_err());
        assert!(compile_filter(&serde_json::json!(["let", 3, ["get", "x"], ["has", "y"]]), Scope::root()).is_err());
    }

    /// Acceptance, on the real layer: `poi-label` compiles end to end out of
    /// the vendored document and keeps its maki icon, which is the whole
    /// point of un-skipping it.
    #[test]
    fn the_real_poi_label_layer_compiles_with_its_icon() {
        let layer = or_skip!(standard_dark_layer("poi-label"));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("poi-label must no longer skip");
        assert_eq!(compiled.len(), 1);
        let StyleLayer::Symbol { icon_image, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let restaurant = tags(&[("maki", "restaurant")]);
        let icon = icon_image.as_ref().expect("poi-label's icon must survive the compile");
        assert_eq!(icon.resolve(&restaurant, 16).as_deref(), Some("restaurant-11"));
        assert_eq!(icon.resolve(&restaurant, 17).as_deref(), Some("restaurant-15"));
    }

    /// The user-visible defect this property was compiled for: at z15 this
    /// renderer drew roughly eighteen POI and transit labels and icons that
    /// Mapbox GL JS draws NONE of, because `poi-label`'s real
    /// `text-opacity` ramp -- `["step", ["zoom"], 0, 16, 0, 17, 1]` -- was
    /// not read at all and every label defaulted to fully opaque from the
    /// layer's `minzoom` of 6.
    #[test]
    fn the_real_poi_labels_text_opacity_hides_it_until_z17() {
        let layer = or_skip!(standard_dark_layer("poi-label"));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("poi-label must compile");
        let StyleLayer::Symbol { text_opacity, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let restaurant = tags(&[("maki", "restaurant")]);
        for zoom in [6, 10, 14, 15, 16] {
            assert_eq!(text_opacity.resolve(&restaurant, zoom), 0.0, "poi text must be invisible at z{zoom}");
        }
        for zoom in [17, 18, 22] {
            assert_eq!(text_opacity.resolve(&restaurant, zoom), 1.0, "poi text must be visible at z{zoom}");
        }
    }

    /// `poi-label`'s real `icon-opacity`,
    /// `["step", ["zoom"], ["step", ["get", "sizerank"], 0, 10, 0], 17, 1]`
    /// -- a zoom step whose base is itself a step bucketing features by a
    /// NUMERIC tag. Nothing compiled that shape before, so the whole
    /// property degraded to the spec default of 1 and every POI icon drew
    /// from z6.
    ///
    /// Both buckets read 0 below z17 here (this layer's inner step says the
    /// same thing twice), and the point of the test is that they say it for
    /// a feature with a `sizerank` on either side of the threshold AND for
    /// one carrying no `sizerank` at all.
    #[test]
    fn the_real_poi_labels_icon_opacity_hides_it_until_z17_at_every_sizerank() {
        let layer = or_skip!(standard_dark_layer("poi-label"));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("poi-label must compile");
        let StyleLayer::Symbol { icon_opacity, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let ranked = |rank: f64| HashMap::from([("sizerank".to_string(), TagValue::Number(rank))]);
        for feature in [ranked(1.0), ranked(10.0), ranked(30.0), HashMap::new()] {
            for zoom in [6, 12, 16] {
                assert_eq!(icon_opacity.resolve(&feature, zoom), 0.0, "z{zoom}, {feature:?}");
            }
            assert_eq!(icon_opacity.resolve(&feature, 17), 1.0, "z17, {feature:?}");
        }
    }

    /// The same nested shape as `poi-label`'s, but with buckets that
    /// genuinely DISAGREE -- Mapbox Streets' and the SWAT styles' own
    /// `poi-label`/`natural-point-label` `icon-opacity`. A major POI's icon
    /// appears immediately; a minor one waits for z17.
    ///
    /// The missing-`sizerank` case is the load-bearing one: real `step`
    /// semantics put a null input in the BASE bucket, so such a feature must
    /// read 0 below z17 rather than falling out of every bucket and losing
    /// its symbol. See `compile_numerically_bucketed_step`.
    #[test]
    fn a_property_bucketing_icon_opacity_gives_each_bucket_its_own_zoom_ramp() {
        let json = serde_json::json!({
            "id": "poi-label", "type": "symbol", "source-layer": "poi_label",
            "layout": { "icon-image": ["get", "maki"] },
            "paint": { "icon-opacity": ["step", ["zoom"],
                ["step", ["get", "sizerank"], 0, 5, 1], 17, ["step", ["get", "sizerank"], 0, 13, 1]] }
        });
        let compiled = compile_symbol_layer(&json, Scope::root()).expect("the layer must compile");
        assert_eq!(compiled.len(), 1, "a per-feature opacity must NOT expand the layer");
        let StyleLayer::Symbol { icon_opacity, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let ranked = |rank: f64| HashMap::from([("sizerank".to_string(), TagValue::Number(rank))]);
        // sizerank < 5: hidden at every zoom.
        assert_eq!(icon_opacity.resolve(&ranked(2.0), 16), 0.0);
        assert_eq!(icon_opacity.resolve(&ranked(2.0), 17), 0.0);
        // 5 <= sizerank < 13: visible below z17, hidden from z17.
        assert_eq!(icon_opacity.resolve(&ranked(8.0), 16), 1.0);
        assert_eq!(icon_opacity.resolve(&ranked(8.0), 17), 0.0);
        // sizerank >= 13: visible at every zoom.
        assert_eq!(icon_opacity.resolve(&ranked(20.0), 16), 1.0);
        assert_eq!(icon_opacity.resolve(&ranked(20.0), 17), 1.0);
        // No `sizerank` at all lands in the base bucket, exactly as a real
        // `step` on a null input does -- NOT outside every bucket.
        assert_eq!(icon_opacity.resolve(&HashMap::new(), 16), 0.0);
        assert_eq!(icon_opacity.resolve(&HashMap::new(), 17), 0.0);
    }

    /// `transit-stop-label`'s real `icon-opacity`, a zoom step with a
    /// `match` on a STRING tag inside it: bus stops wait for z16, every
    /// other mode appears at z15.5. Proves the `match` half of the paint
    /// path survives being resolved per feature instead of cross-producted
    /// into extra layers.
    #[test]
    fn the_real_transit_stop_labels_icon_opacity_holds_bus_icons_back_a_zoom() {
        let layer = or_skip!(standard_dark_layer("transit-stop-label"));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("transit-stop-label must compile");
        assert_eq!(compiled.len(), 1, "a per-feature opacity must NOT expand the layer");
        let StyleLayer::Symbol { icon_opacity, text_opacity, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let bus = tags(&[("mode", "bus")]);
        let rail = tags(&[("mode", "rail")]);
        assert_eq!(icon_opacity.resolve(&bus, 14), 0.0);
        assert_eq!(icon_opacity.resolve(&rail, 14), 0.0);
        // The `15.5` and `16.5` thresholds land on integer buckets 15 and 16
        // (`ZoomValue`'s stops are integers), so this is the bucket where the
        // two modes part company.
        assert_eq!(icon_opacity.resolve(&bus, 15), 0.0);
        assert_eq!(icon_opacity.resolve(&rail, 15), 1.0);
        assert_eq!(icon_opacity.resolve(&bus, 17), 1.0);
        // The text half is its own ramp and does not follow the icon's.
        assert_eq!(text_opacity.resolve(&bus, 15), 0.0);
        assert_eq!(text_opacity.resolve(&bus, 16), 1.0);
    }

    /// An opacity shape this crate cannot read must cost the layer its
    /// OPACITY, never its symbols: `compile_layer`'s `"symbol"` arm skips a
    /// whole layer on `Err`, and a POI layer with no labels and no icons is
    /// a worse map than one drawn a little too boldly. Same trade
    /// `text-anchor` and `line-dasharray` already make.
    #[test]
    fn an_unreadable_symbol_opacity_degrades_to_opaque_and_keeps_the_layer() {
        let json = serde_json::json!({
            "id": "poi-label", "type": "symbol", "source-layer": "poi_label",
            "layout": { "text-field": ["get", "name"] },
            "paint": { "text-opacity": ["step", ["zoom"], ["get", "opacity_please"], 17, 1] }
        });
        let _ = take_degradations();
        let compiled = compile_symbol_layer(&json, Scope::root()).expect("the layer must survive");
        assert_eq!(compiled.len(), 1);
        let StyleLayer::Symbol { text_opacity, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        assert_eq!(text_opacity.resolve(&HashMap::new(), 10), 1.0);
        let degradations = take_degradations();
        assert!(
            degradations.iter().any(|n| n.contains("text-opacity")),
            "the fallback must be recorded, not silent: {degradations:?}"
        );
    }

    #[test]
    fn compiles_a_constant_layout_value() {
        let json = serde_json::json!(14.0);
        assert!(matches!(compile_layout_value(&json).unwrap(), crate::style::LayoutValue::Constant(v) if v == 14.0));
    }

    #[test]
    fn compiles_the_real_composite_poi_label_text_size_shape() {
        let json = serde_json::json!(["step", ["zoom"],
            ["step", ["get", "sizerank"], 18, 5, 12],
            17,
            ["step", ["get", "sizerank"], 18, 13, 12]
        ]);
        let v = compile_layout_value(&json).unwrap();
        assert!(matches!(v, crate::style::LayoutValue::ZoomOfDataStep(_)));
    }

    #[test]
    fn compiles_the_real_composite_poi_label_text_offset_shape() {
        // poi-label's real text-offset expression:
        // step(zoom, step(sizerank, [0,0], 5, [0,0.8]), 17, step(sizerank, [0,0], 13, [0,0.8]))
        let json = serde_json::json!(["step", ["zoom"],
            ["step", ["get", "sizerank"], ["literal", [0, 0]], 5, ["literal", [0, 0.8]]],
            17,
            ["step", ["get", "sizerank"], ["literal", [0, 0]], 13, ["literal", [0, 0.8]]]
        ]);
        let (x, y) = compile_layout_value_pair(&json).unwrap();
        assert!(matches!(x, crate::style::LayoutValue::ZoomOfDataStep(_)));
        assert!(matches!(y, crate::style::LayoutValue::ZoomOfDataStep(_)));

        // Below the z17 bracket, sizerank 2 is below the 5-threshold ->
        // base [0, 0].
        let mut low_rank = HashMap::new();
        low_rank.insert("sizerank".to_string(), TagValue::Number(2.0));
        assert_eq!(x.resolve(&low_rank, 10), 0.0);
        assert_eq!(y.resolve(&low_rank, 10), 0.0);

        // Below the z17 bracket, sizerank 8 clears the 5-threshold -> [0, 0.8].
        let mut high_rank = HashMap::new();
        high_rank.insert("sizerank".to_string(), TagValue::Number(8.0));
        assert_eq!(x.resolve(&high_rank, 10), 0.0);
        assert_eq!(y.resolve(&high_rank, 10), 0.8);
    }

    #[test]
    fn defaults_a_missing_opacity_to_one_leaving_the_colors_own_alpha_unchanged() {
        let json = br##"{"layers": [
            {
                "id": "fill-no-opacity", "type": "fill", "source-layer": "landuse",
                "paint": {"fill-color": "hsla(0, 0%, 0%, 0.42)"}
            },
            {
                "id": "line-no-opacity", "type": "line", "source-layer": "transportation",
                "paint": {"line-color": "hsla(0, 0%, 0%, 0.73)"}
            }
        ]}"##;
        let style = parse_and_compile(json).unwrap();
        assert_eq!(style.layers.len(), 2);

        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![
                Layer {
                    name: "landuse".to_string(),
                    features: vec![Feature { geometry: polygon_geometry(), tags: Default::default() }],
                },
                Layer {
                    name: "transportation".to_string(),
                    features: vec![Feature { geometry: line_geometry(), tags: Default::default() }],
                },
            ],
        };
        let plan = match_tile(&tile, &style, 10);
        assert_eq!(plan.ops.len(), 2);

        let fill_alpha = plan
            .ops
            .iter()
            .find_map(|op| match op {
                PaintOp::Fill { color_from, .. } => Some(color_from.a),
                _ => None,
            })
            .expect("fill op present");
        assert!((fill_alpha - 0.42).abs() < 1e-3, "fill alpha should equal the color's own alpha unchanged, got {fill_alpha}");

        let line_alpha = plan
            .ops
            .iter()
            .find_map(|op| match op {
                PaintOp::Line { color_from, .. } => Some(color_from.a),
                _ => None,
            })
            .expect("line op present");
        assert!((line_alpha - 0.73).abs() < 1e-3, "line alpha should equal the color's own alpha unchanged, got {line_alpha}");
    }

    #[test]
    fn compiles_the_real_poi_label_symbol_layer() {
        let json = serde_json::json!({
            "id": "poi-label",
            "type": "symbol",
            "source-layer": "poi_label",
            "minzoom": 6,
            "layout": {
                "text-field": ["coalesce", ["get", "name_en"], ["get", "name"]],
                "text-size": ["step", ["zoom"],
                    ["step", ["get", "sizerank"], 18, 5, 12],
                    17,
                    ["step", ["get", "sizerank"], 18, 13, 12]
                ],
                "text-offset": ["literal", [0, 0.8]],
                "icon-image": ["get", "maki"]
            },
            "paint": {
                "text-color": "#666",
                "text-halo-color": "#fff",
                "text-halo-width": 1.0
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).unwrap();
        assert_eq!(layers.len(), 1, "a plain (non-match) text-color must compile to exactly one StyleLayer::Symbol");
        match &layers[0] {
            StyleLayer::Symbol { id, source_layer, min_zoom, text_field, icon_image, .. } => {
                assert_eq!(id, "poi-label");
                assert_eq!(source_layer, "poi_label");
                assert_eq!(*min_zoom, Some(6));
                assert!(text_field.is_some());
                assert!(icon_image.is_some());
            }
            other => panic!("expected StyleLayer::Symbol, got {other:?}"),
        }
    }

    #[test]
    fn compiles_the_real_full_poi_label_layer_end_to_end() {
        // The real vendored Mapbox Streets v12 `poi-label` layer (extracted
        // via `python3 -c "import json; print(json.dumps([l for l in
        // json.load(open('examples/mapbox-streets-style.json'))['layers']
        // if l['id']=='poi-label'][0]))"`), trimmed to only the pieces this
        // compiler reads: the real `"filter"` (zoom-offset arithmetic
        // comparison, fixed by 810cc76), the real `icon-image` (case-wrapped
        // maki_beta/maki coalesce, fixed by d748102), the real `text-field`,
        // `text-size`, and `text-offset` (the zoom-of-data-step composite,
        // the latter with `["literal", [x, y]]` leaves -- this change's
        // `compile_layout_value_pair`). `text-font`/`text-anchor`/
        // `icon-opacity`/`text-halo-blur`/`metadata` are dropped: real
        // properties this compiler never reads on ANY layer (see the
        // module-level doc comment's "silently skipped" case), not
        // something specific to poi-label. `paint.text-color` here is
        // trimmed to a plain CSS color rather than poi-label's real
        // `match`-on-`class` expression -- that match-expression case is
        // exercised separately by
        // `compiles_the_real_poi_label_text_color_match_expression` below
        // (the gap this test's comment used to describe, where
        // `compile_symbol_layer`'s `text-color` called `color_leaf` directly
        // and rejected `match`, is now fixed via `compile_zoom_value`; see
        // the vendored-style regression test's step-9 comment). This test's
        // job is proving the other properties this change and the two
        // earlier commits actually fixed now combine correctly on the real
        // layer shape.
        let json = serde_json::json!({
            "id": "poi-label",
            "type": "symbol",
            "source-layer": "poi_label",
            "minzoom": 6,
            "filter": ["<=", ["get", "filterrank"], ["+", ["step", ["zoom"], 0, 16, 1, 17, 2], 3]],
            "layout": {
                "text-size": ["step", ["zoom"],
                    ["step", ["get", "sizerank"], 18, 5, 12],
                    17,
                    ["step", ["get", "sizerank"], 18, 13, 12]
                ],
                "icon-image": [
                    "case",
                    ["has", "maki_beta"],
                    ["coalesce", ["image", ["get", "maki_beta"]], ["image", ["get", "maki"]]],
                    ["image", ["get", "maki"]]
                ],
                "text-offset": ["step", ["zoom"],
                    ["step", ["get", "sizerank"], ["literal", [0, 0]], 5, ["literal", [0, 0.8]]],
                    17,
                    ["step", ["get", "sizerank"], ["literal", [0, 0]], 13, ["literal", [0, 0.8]]]
                ],
                "text-field": ["coalesce", ["get", "name_en"], ["get", "name"]]
            },
            "paint": {
                "text-halo-color": "hsl(20, 20%, 100%)",
                "text-halo-width": 0.5,
                "text-color": "hsl(210, 20%, 46%)"
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("the real (trimmed) poi-label layer must compile end-to-end");
        assert_eq!(layers.len(), 1, "a plain (non-match) text-color must compile to exactly one StyleLayer::Symbol");
        match &layers[0] {
            StyleLayer::Symbol { id, source_layer, min_zoom, text_field, icon_image, text_offset_x, text_offset_y, .. } => {
                assert_eq!(id, "poi-label");
                assert_eq!(source_layer, "poi_label");
                assert_eq!(*min_zoom, Some(6));
                assert!(text_field.is_some());
                assert!(icon_image.is_some());
                assert!(matches!(text_offset_x, crate::style::LayoutValue::ZoomOfDataStep(_)));
                assert!(matches!(text_offset_y, crate::style::LayoutValue::ZoomOfDataStep(_)));

                let mut high_rank = HashMap::new();
                high_rank.insert("sizerank".to_string(), TagValue::Number(8.0));
                assert_eq!(text_offset_x.resolve(&high_rank, 10), 0.0);
                assert_eq!(text_offset_y.resolve(&high_rank, 10), 0.8);
            }
            other => panic!("expected StyleLayer::Symbol, got {other:?}"),
        }
    }

    #[test]
    fn compiles_the_real_poi_label_text_color_match_expression() {
        // The real Mapbox Streets v12 `poi-label` `paint.text-color`: a
        // `match` on `class` with several single-string cases, a couple of
        // multi-value cases (`["store_like", "food_and_drink_stores"]` etc.),
        // and a default -- confirmed via `python3 -c "import json; print(...
        // paint['text-color'])"` against the vendored
        // `examples/mapbox-streets-style.json`. A representative excerpt
        // (first two real cases plus the real default) is enough to prove
        // the `match`-expansion path this change adds; it doesn't need every
        // real case (that's covered end-to-end by the full vendored-style
        // regression test below, where `poi-label` compiles with ALL real
        // cases).
        let json = serde_json::json!({
            "id": "poi-label",
            "type": "symbol",
            "source-layer": "poi_label",
            "minzoom": 6,
            "filter": ["<=", ["get", "filterrank"], ["+", ["step", ["zoom"], 0, 16, 1, 17, 2], 3]],
            "layout": {
                "text-field": ["coalesce", ["get", "name_en"], ["get", "name"]],
                "icon-image": ["get", "maki"]
            },
            "paint": {
                "text-color": ["match", ["get", "class"],
                    "food_and_drink", "hsl(40, 95%, 43%)",
                    "park_like", "hsl(110, 70%, 28%)",
                    "hsl(210, 20%, 46%)"
                ]
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("poi-label's real text-color match expression must compile");
        assert_eq!(
            layers.len(),
            3,
            "expected one StyleLayer::Symbol per text-color case (food_and_drink, park_like, default)"
        );
        for layer in &layers {
            match layer {
                StyleLayer::Symbol { id, .. } => assert_eq!(id, "poi-label"),
                other => panic!("expected StyleLayer::Symbol, got {other:?}"),
            }
        }

        let expected_food = parse_css_color("hsl(40, 95%, 43%)").unwrap();
        let expected_park = parse_css_color("hsl(110, 70%, 28%)").unwrap();
        let expected_default = parse_css_color("hsl(210, 20%, 46%)").unwrap();

        // Each case's filter ANDs the class match with poi-label's own
        // filterrank filter: a food_and_drink feature at a filterrank that
        // fails the layer's own filter should match no expanded case.
        let mut food_tags = HashMap::new();
        food_tags.insert("class".to_string(), TagValue::String("food_and_drink".to_string()));
        food_tags.insert("filterrank".to_string(), TagValue::Number(2.0));

        let mut park_tags = HashMap::new();
        park_tags.insert("class".to_string(), TagValue::String("park_like".to_string()));
        park_tags.insert("filterrank".to_string(), TagValue::Number(2.0));

        let mut other_tags = HashMap::new();
        other_tags.insert("class".to_string(), TagValue::String("some_unlisted_class".to_string()));
        other_tags.insert("filterrank".to_string(), TagValue::Number(2.0));

        let matching_color = |tags: &HashMap<String, TagValue>| -> Option<crate::style::Rgba> {
            layers.iter().find_map(|layer| match layer {
                StyleLayer::Symbol { filter, text_color, .. } if filter.matches(tags, 10.0) => {
                    Some(text_color.value_at(10))
                }
                _ => None,
            })
        };

        let food_color = matching_color(&food_tags).expect("food_and_drink feature should match a poi-label case");
        assert_eq!((food_color.r, food_color.g, food_color.b, food_color.a), (expected_food.r, expected_food.g, expected_food.b, expected_food.a));

        let park_color = matching_color(&park_tags).expect("park_like feature should match a poi-label case");
        assert_eq!((park_color.r, park_color.g, park_color.b, park_color.a), (expected_park.r, expected_park.g, expected_park.b, expected_park.a));

        let default_color = matching_color(&other_tags).expect("an unlisted class should match poi-label's default case");
        assert_eq!(
            (default_color.r, default_color.g, default_color.b, default_color.a),
            (expected_default.r, expected_default.g, expected_default.b, expected_default.a)
        );
    }

    #[test]
    fn compiles_icon_size_and_defaults_it_to_one() {
        let with_icon_size = serde_json::json!({
            "id": "with-icon-size",
            "type": "symbol",
            "source-layer": "poi_label",
            "layout": {
                "icon-image": ["get", "maki"],
                "icon-size": ["step", ["zoom"], 0.25, 17, 0.5]
            }
        });
        let with_icon_size_layers = compile_symbol_layer(&with_icon_size, Scope::root()).unwrap();
        assert_eq!(with_icon_size_layers.len(), 1);
        match &with_icon_size_layers[0] {
            StyleLayer::Symbol { icon_size, .. } => {
                assert_eq!(icon_size.resolve(&HashMap::new(), 16), 0.25);
                assert_eq!(icon_size.resolve(&HashMap::new(), 18), 0.5);
            }
            other => panic!("expected StyleLayer::Symbol, got {other:?}"),
        }

        let without = serde_json::json!({
            "id": "no-icon-size",
            "type": "symbol",
            "source-layer": "poi_label",
            "layout": {"icon-image": ["get", "maki"]}
        });
        let without_layers = compile_symbol_layer(&without, Scope::root()).unwrap();
        assert_eq!(without_layers.len(), 1);
        match &without_layers[0] {
            StyleLayer::Symbol { icon_size, .. } => {
                // Mapbox's own default: draw the sprite at its native size.
                assert_eq!(icon_size.resolve(&HashMap::new(), 16), 1.0);
            }
            other => panic!("expected StyleLayer::Symbol, got {other:?}"),
        }
    }

    #[test]
    fn skips_a_zoom_stepped_placement_symbol_layer_with_an_unknown_keyword() {
        // Both constant and zoom-stepped "line" placement now compile (see
        // `compiles_a_zoom_stepped_symbol_placement`); an unknown keyword ("line-middle") stays
        // out of scope in either shape and must still `Err`.
        let json = serde_json::json!({
            "id": "road-label",
            "type": "symbol",
            "source-layer": "road",
            "layout": {"symbol-placement": ["step", ["zoom"], "point", 11, "line-middle"], "text-field": ["get", "name"]},
        });
        assert!(compile_symbol_layer(&json, Scope::root()).is_err());
    }

    #[test]
    fn symbol_layer_with_neither_text_field_nor_icon_image_is_an_error() {
        let json = serde_json::json!({
            "id": "empty-symbol",
            "type": "symbol",
            "source-layer": "poi_label",
            "layout": {}
        });
        assert!(compile_symbol_layer(&json, Scope::root()).is_err());
    }

    #[test]
    fn compile_layer_dispatches_symbol_layers_and_skips_unsupported_ones_softly() {
        let json = serde_json::json!({
            "layers": [
                {
                    "id": "poi-label",
                    "type": "symbol",
                    "source-layer": "poi_label",
                    "layout": {"text-field": ["get", "name"]},
                },
                {
                    "id": "road-label",
                    "type": "symbol",
                    "source-layer": "road",
                    // A zoom-stepped placement with an unknown keyword, not
                    // constant/stepped "point"/"line" (which now both
                    // compile), so this layer keeps `Err`ing and stands in
                    // for "an out-of-scope construct" for this dispatch test.
                    "layout": {"symbol-placement": ["step", ["zoom"], "point", 11, "line-middle"], "text-field": ["get", "name"]},
                },
            ]
        });
        let style = parse_and_compile(json.to_string().as_bytes()).unwrap();
        assert_eq!(style.layers.len(), 1, "the unsupported-placement symbol layer should be softly skipped, not hard-fail the whole style");
        assert!(matches!(&style.layers[0], StyleLayer::Symbol { id, .. } if id == "poi-label"));
    }

    /// The three `interpolate` expressions below are copied VERBATIM out of
    /// `examples/mapbox-streets-style.json` (`crosswalks`' `icon-size`,
    /// `road-intersection`'s `text-size`, `continent-label`'s `text-size`).
    /// Before this change every one of them was rejected by
    /// `compile_layout_value` with `unsupported layout value expression
    /// shape: ...`, which soft-skipped the whole layer -- verified by
    /// running the compiler over each layer in isolation, not guessed.
    #[test]
    fn compiles_the_real_crosswalks_linear_interpolate_icon_size_curve() {
        // ["interpolate", ["linear"], ["zoom"], 16, 0.1, 18, 0.2, 19, 0.5, 22, 1.5]
        let json = serde_json::json!(
            ["interpolate", ["linear"], ["zoom"], 16, 0.1, 18, 0.2, 19, 0.5, 22, 1.5]
        );
        let value = compile_layout_value(&json).expect("a zoom interpolate must compile");
        let at = |z: u8| value.resolve(&HashMap::new(), z);

        // Every declared stop must come back exactly.
        assert!((at(16) - 0.1).abs() < 1e-5, "z16 stop, got {}", at(16));
        assert!((at(18) - 0.2).abs() < 1e-5, "z18 stop, got {}", at(18));
        assert!((at(19) - 0.5).abs() < 1e-5, "z19 stop, got {}", at(19));
        assert!((at(22) - 1.5).abs() < 1e-5, "z22 stop, got {}", at(22));

        // The point of the whole change: z17 is a TRUE interior sample of
        // the curve, not a clamp to either bracketing stop. Linear between
        // (16, 0.1) and (18, 0.2) puts it at exactly 0.15 -- if the curve
        // were being stepped or clamped instead, this would read 0.1.
        assert!((at(17) - 0.15).abs() < 1e-5, "z17 must be the linear midpoint 0.15, got {}", at(17));
        assert!(at(17) > at(16) && at(17) < at(18), "z17 must lie strictly inside its bracket");

        // Outside the declared range the style spec clamps to the end
        // stops rather than extrapolating.
        assert!((at(10) - 0.1).abs() < 1e-5, "below the first stop clamps, got {}", at(10));
    }

    #[test]
    fn compiles_the_real_road_intersection_exponential_interpolate_text_size_curve() {
        // ["interpolate", ["exponential", 1.2], ["zoom"], 15, 9, 18, 12]
        let json =
            serde_json::json!(["interpolate", ["exponential", 1.2], ["zoom"], 15, 9, 18, 12]);
        let value = compile_layout_value(&json).expect("a zoom interpolate must compile");
        let at = |z: u8| value.resolve(&HashMap::new(), z);

        assert!((at(15) - 9.0).abs() < 1e-4, "z15 stop, got {}", at(15));
        assert!((at(18) - 12.0).abs() < 1e-4, "z18 stop, got {}", at(18));

        // z16 and z17 must be true interior samples of the EXPONENTIAL
        // curve (base 1.2), strictly between the endpoints and strictly
        // increasing -- this is what proves the curve is sampled rather
        // than clamped to an endpoint or flattened to a step.
        for z in [16u8, 17] {
            assert!(at(z) > 9.0 && at(z) < 12.0, "z{z} must be strictly inside (9, 12), got {}", at(z));
        }
        assert!(at(16) < at(17), "the curve must be monotonically increasing");

        // The exact MapLibre exponential formula:
        // t = (1.2^(z-15) - 1) / (1.2^3 - 1), value = 9 + 3t.
        let expected = |z: f64| {
            let t = (1.2f64.powf(z - 15.0) - 1.0) / (1.2f64.powf(3.0) - 1.0);
            (9.0 + 3.0 * t) as f32
        };
        assert!((at(16) - expected(16.0)).abs() < 1e-4, "z16 got {} want {}", at(16), expected(16.0));
        assert!((at(17) - expected(17.0)).abs() < 1e-4, "z17 got {} want {}", at(17), expected(17.0));
    }

    #[test]
    fn compiles_the_real_continent_label_sub_one_exponential_base_text_size_curve() {
        // ["interpolate", ["exponential", 0.5], ["zoom"], 0, 10, 2.5, 15]
        // A base BELOW 1 (front-loaded growth) and a FRACTIONAL final stop
        // zoom -- both real, and both handled by the paint-side compiler's
        // resample-at-every-integer-zoom approach.
        let json = serde_json::json!(["interpolate", ["exponential", 0.5], ["zoom"], 0, 10, 2.5, 15]);
        let value = compile_layout_value(&json).expect("a zoom interpolate must compile");
        let at = |z: u8| value.resolve(&HashMap::new(), z);

        assert!((at(0) - 10.0).abs() < 1e-4, "z0 stop, got {}", at(0));
        // The last stop is at the fractional zoom 2.5, so z3 (the first
        // integer at or past it) clamps to that stop's value.
        assert!((at(3) - 15.0).abs() < 1e-4, "z3 clamps to the z2.5 stop, got {}", at(3));

        // z1 and z2 are true interior samples.
        for z in [1u8, 2] {
            assert!(at(z) > 10.0 && at(z) < 15.0, "z{z} must be strictly inside (10, 15), got {}", at(z));
        }
        assert!(at(1) < at(2), "the curve must be monotonically increasing");

        // A base below 1 grows FASTER than linear early on: the linear
        // midpoint of the 0..2.5 span at z1 would be 10 + 5*(1/2.5) = 12,
        // and the base-0.5 curve must sit above it. This is what proves the
        // declared exponential base is actually honored rather than the
        // curve degrading to linear.
        assert!(at(1) > 12.0, "base 0.5 must be front-loaded, expected > 12 at z1, got {}", at(1));
    }

    #[test]
    fn compiles_a_property_varying_interpolate_layout_value_whose_stops_are_string_matches() {
        // The "jointly varying" shape: an interpolate whose stop OUTPUTS
        // are themselves `match` expressions on a STRING feature property.
        // The paint-side compiler expands a paint-property `match` into one
        // (Filter, ZoomValue) pair per case, which `LayoutValue` cannot
        // carry (no per-case filter) -- but `LayoutValue::ZoomOfMatchStep`
        // (via `compile_match_step_stops`) represents THIS shape directly:
        // real MapLibre evaluates a `match` sub-expression fresh at each
        // bracketing stop, per feature, then blends the two resulting
        // numbers by the zoom curve (`Interpolate.evaluate`,
        // `@maplibre/maplibre-gl-style-spec`
        // `src/expression/definitions/interpolate.ts`), and that is exactly
        // what `match_step_at` does at every sampled integer zoom.
        let json = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            10, ["match", ["get", "class"], "major", 12, 8],
            14, ["match", ["get", "class"], "major", 18, 11]
        ]);
        let value = compile_layout_value(&json).expect("a match-shaped property-varying interpolate now compiles");
        let major = string_tag("class", "major"); // matches the one arm
        let minor = string_tag("class", "minor"); // matches no arm's labels
        let missing: HashMap<String, TagValue> = HashMap::new(); // no "class" tag at all

        // At the first declared stop: the arm's value, and the DEFAULT --
        // never arm 1's value -- for anything that doesn't match it.
        assert!((value.resolve(&major, 10) - 12.0).abs() < 1e-3, "got {}", value.resolve(&major, 10));
        assert!(
            (value.resolve(&minor, 10) - 8.0).abs() < 1e-3,
            "an unmatched feature must take the DEFAULT (8), not arm 1's value (12): got {}",
            value.resolve(&minor, 10)
        );
        assert!(
            (value.resolve(&missing, 10) - 8.0).abs() < 1e-3,
            "a missing tag must take the default too, got {}",
            value.resolve(&missing, 10)
        );

        // At the second declared stop.
        assert!((value.resolve(&major, 14) - 18.0).abs() < 1e-3, "got {}", value.resolve(&major, 14));
        assert!((value.resolve(&minor, 14) - 11.0).abs() < 1e-3, "got {}", value.resolve(&minor, 14));

        // Halfway between the two stops (z12 on a ["linear"] curve: t=0.5).
        assert!((value.resolve(&major, 12) - 15.0).abs() < 1e-3, "got {}", value.resolve(&major, 12));
        assert!((value.resolve(&minor, 12) - 9.5).abs() < 1e-3, "got {}", value.resolve(&minor, 12));
    }

    /// VERBATIM from `road-label`'s real `text-size` in
    /// `examples/mapbox-streets-style.json`: two labelled arms (5 major road
    /// classes, 7 link/street classes) plus a bare default, at two zoom
    /// stops. Covers the checklist `compile_match_step_stops` promises: a
    /// feature matching arm 1, a feature matching a LATER arm, an unmatched
    /// feature taking the default, and correct interpolation at a zoom
    /// between the two declared stops.
    #[test]
    fn compiles_the_real_road_label_text_size_and_resolves_every_arm() {
        let json = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            10, ["match", ["get", "class"],
                 ["motorway", "trunk", "primary", "secondary", "tertiary"], 10,
                 ["motorway_link", "trunk_link", "primary_link", "secondary_link",
                  "tertiary_link", "street", "street_limited"], 9,
                 6.5],
            18, ["match", ["get", "class"],
                 ["motorway", "trunk", "primary", "secondary", "tertiary"], 16,
                 ["motorway_link", "trunk_link", "primary_link", "secondary_link",
                  "tertiary_link", "street", "street_limited"], 14,
                 13]
        ]);
        let value = compile_layout_value(&json).expect("road-label's real text-size now compiles");

        // Arm 1 (a major road class).
        let major = string_tag("class", "primary");
        assert!((value.resolve(&major, 10) - 10.0).abs() < 1e-3);
        assert!((value.resolve(&major, 18) - 16.0).abs() < 1e-3);

        // Arm 2 -- a LATER arm than arm 1, proving arm order/lookup, not
        // just "the first arm", is honored.
        let link = string_tag("class", "street_limited");
        assert!((value.resolve(&link, 10) - 9.0).abs() < 1e-3);
        assert!((value.resolve(&link, 18) - 14.0).abs() < 1e-3);

        // Unmatched: a real `road` class this filter admits (`path`) that
        // is in neither arm, so it must fall to the bare default -- never
        // arm 1's, and never zero.
        let unmatched = string_tag("class", "path");
        assert!((value.resolve(&unmatched, 10) - 6.5).abs() < 1e-3, "got {}", value.resolve(&unmatched, 10));
        assert!((value.resolve(&unmatched, 18) - 13.0).abs() < 1e-3, "got {}", value.resolve(&unmatched, 18));

        // Intermediate zoom (z14 is exactly halfway between z10 and z18 on
        // a ["linear"] curve, t=0.5), for all three buckets at once.
        assert!((value.resolve(&major, 14) - 13.0).abs() < 1e-3, "got {}", value.resolve(&major, 14));
        assert!((value.resolve(&link, 14) - 11.5).abs() < 1e-3, "got {}", value.resolve(&link, 14));
        assert!((value.resolve(&unmatched, 14) - 9.75).abs() < 1e-3, "got {}", value.resolve(&unmatched, 14));
    }

    /// The DataStepShape numeric path must stay exactly as it was: this is
    /// the same expression `compiles_the_real_state_label_cubic_bezier_over_symbolrank_step_stops`
    /// already exercises, re-asserted here to pin that adding the
    /// `MatchStepShape`/match dispatch above touched nothing on the numeric
    /// side (same function, same dispatch point).
    #[test]
    fn the_numeric_data_step_shaped_interpolate_stops_are_unaffected_by_the_match_dispatch() {
        let json = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            4, ["step", ["get", "symbolrank"], 9, 6, 8],
            9, ["step", ["get", "symbolrank"], 21, 6, 16]
        ]);
        let value = compile_layout_value(&json).expect("a step-shaped property-varying interpolate still compiles");
        assert!((value.resolve(&numeric_tag("symbolrank", 6.0), 4) - 8.0).abs() < 1e-3);
        assert!((value.resolve(&numeric_tag("symbolrank", 6.0), 9) - 16.0).abs() < 1e-3);
        assert!((value.resolve(&numeric_tag("symbolrank", 3.0), 4) - 9.0).abs() < 1e-3, "below every threshold: base");
    }

    #[test]
    fn refuses_interpolate_stops_that_mix_step_and_match_shapes() {
        // A stop list must be uniformly ONE per-feature shape: pairing a
        // numeric `>=`-threshold bucket with a string-label bucket has no
        // defensible bucket correspondence, so this stays an `Err` rather
        // than a guess at which reading to prefer.
        let json = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            4, ["step", ["get", "class"], 9, 6, 8],
            9, ["match", ["get", "class"], "major", 21, 16]
        ]);
        let err = compile_layout_value(&json).expect_err("mixed step/match stops must be refused");
        assert!(err.to_string().contains("SAME"), "the error must say the shapes must match, got {err}");
    }

    #[test]
    fn refuses_match_shaped_interpolate_stops_whose_label_groups_disagree_across_stops() {
        // Unlike a numeric `step`'s thresholds (which can be losslessly
        // unified onto their union, see `refine_data_step_onto_thresholds`),
        // two `match`es that partition the same property into DIFFERENT
        // groups have no lossless common refinement to reconcile onto --
        // splitting/merging label groups is a real disagreement about what
        // the buckets ARE, not just how finely they're drawn.
        let json = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            10, ["match", ["get", "class"], ["motorway", "trunk"], 10, 6],
            18, ["match", ["get", "class"], ["motorway"], 16, ["trunk"], 12, 6]
        ]);
        let err = compile_layout_value(&json).expect_err("disagreeing label groups must be refused");
        assert!(err.to_string().contains("label groups"), "the error must name the disagreement, got {err}");
    }

    #[test]
    fn rejects_a_non_zoom_interpolate_layout_value() {
        // Property-driven interpolation (`["get", ...]` as the input rather
        // than `["zoom"]`) isn't representable by `ZoomValue` at all; the
        // shared paint-side compiler already rejects it, and routing layout
        // values through that same compiler must surface the same refusal.
        let json = serde_json::json!(["interpolate", ["linear"], ["get", "sizerank"], 0, 10, 20, 4]);
        assert!(compile_layout_value(&json).is_err(), "property-input interpolate must be rejected");
    }

    #[test]
    fn a_pair_valued_interpolate_agrees_with_the_scalar_walker_stop_for_stop() {
        // The pair-valued walker used to REFUSE `interpolate` outright, for
        // want of a real style to validate it against. Mapbox Standard's
        // `transit-stop-label` supplied one, and the shape now compiles by
        // rewriting each `["literal", [x, y]]` stop down to its component
        // and delegating to the scalar compiler. The point of this test is
        // that delegation: compiled per component, the pair must agree with
        // the equivalent scalar expression at every zoom -- if it ever did
        // not, the two walkers would have started disagreeing about what a
        // curve means, which is the exact failure the refusal was guarding
        // against.
        let pair = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            10, ["literal", [0, 0]],
            14, ["literal", [0, 0.8]]
        ]);
        let scalar = serde_json::json!(["interpolate", ["linear"], ["zoom"], 10, 0, 14, 0.8]);
        let (x, y) = compile_layout_value_pair(&pair).expect("a zoom-interpolated pair must compile");
        let scalar = compile_layout_value(&scalar).expect("the scalar equivalent compiles today");
        let no_tags = HashMap::new();
        for zoom in 0..=22u8 {
            assert_eq!(x.resolve(&no_tags, zoom), 0.0, "x at z{zoom}");
            assert_eq!(y.resolve(&no_tags, zoom), scalar.resolve(&no_tags, zoom), "y at z{zoom}");
        }
    }

    #[test]
    fn compiles_the_real_crosswalks_and_road_intersection_symbol_layers_end_to_end() {
        // End-to-end: these two REAL vendored Mapbox Streets layers were
        // soft-skipped in their entirety before this change, purely because
        // `compile_layout_value` rejected their `interpolate` layout value.
        let style = or_skip!(mapbox_streets());

        let crosswalks: Vec<_> = style
            .layers
            .iter()
            .filter_map(|l| match l {
                StyleLayer::Symbol { id, icon_size, .. } if id == "crosswalks" => Some(icon_size),
                _ => None,
            })
            .collect();
        assert_eq!(crosswalks.len(), 1, "crosswalks must now compile (into exactly one Symbol layer)");
        // Its real icon-size curve, sampled through the compiled layer.
        assert!((crosswalks[0].resolve(&HashMap::new(), 18) - 0.2).abs() < 1e-5);
        assert!((crosswalks[0].resolve(&HashMap::new(), 17) - 0.15).abs() < 1e-5);

        let road_intersection: Vec<_> = style
            .layers
            .iter()
            .filter_map(|l| match l {
                StyleLayer::Symbol { id, text_size, .. } if id == "road-intersection" => Some(text_size),
                _ => None,
            })
            .collect();
        assert_eq!(road_intersection.len(), 1, "road-intersection must now compile");
        let size = |z: u8| road_intersection[0].resolve(&HashMap::new(), z);
        assert!((size(15) - 9.0).abs() < 1e-4);
        assert!((size(18) - 12.0).abs() < 1e-4);
        assert!(size(16) > 9.0 && size(16) < 12.0, "z16 must be a true interior sample, got {}", size(16));
    }

    /// Builds a minimal-but-real symbol layer around one paint/zoom-bound
    /// override, so a single blocker can be exercised through
    /// `compile_symbol_layer` (the function that actually reads
    /// `text-halo-color` and the zoom bounds) without dragging in an entire
    /// vendored layer's unrelated properties.
    fn symbol_layer_with(extra: serde_json::Value) -> Value {
        let mut json = serde_json::json!({
            "id": "test-label",
            "type": "symbol",
            "source-layer": "natural_label",
            "layout": {"text-field": ["get", "name"]},
        });
        for (key, value) in extra.as_object().expect("overrides must be a JSON object") {
            json[key] = value.clone();
        }
        json
    }

    #[test]
    fn compiles_an_interpolate_shaped_text_halo_color_the_same_way_text_color_is_compiled() {
        // VERBATIM from `continent-label`'s paint in
        // `examples/mapbox-streets-style.json` (`country-label`'s is the
        // same shape with a `2` first stop). `text-halo-color` was read
        // through a bare `color_leaf`, which only accepts a plain CSS color
        // STRING, so any expression-shaped halo color hard-failed the whole
        // symbol layer -- even though `text-color`, three lines above it in
        // the same function, has gone through `compile_zoom_value` since
        // `poi-label`'s `match`-shaped `text-color` was wired up. Four real
        // symbol layers in the vendored style have an expression-shaped
        // `text-halo-color`, so this is a general inconsistency, not a
        // one-layer patch.
        let layer = symbol_layer_with(serde_json::json!({
            "paint": {
                "text-halo-color": [
                    "interpolate", ["linear"], ["zoom"],
                    0, "hsla(20, 25%, 100%, 0.75)",
                    3, "hsl(20, 25%, 100%)"
                ]
            }
        }));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("an interpolate halo color must compile");
        assert_eq!(compiled.len(), 1, "a zoom-only halo curve adds no filtered cases");
        let StyleLayer::Symbol { text_halo_color, .. } = &compiled[0] else {
            panic!("compile_symbol_layer must produce a Symbol layer");
        };

        // Both real stops are the same white; only the ALPHA ramps
        // (0.75 -> 1.0), which is exactly what makes an interior sample
        // distinguishable from a clamp to either endpoint.
        assert!((text_halo_color.value_at(0).a - 0.75).abs() < 1e-4, "z0 stop, got {}", text_halo_color.value_at(0).a);
        assert!((text_halo_color.value_at(3).a - 1.0).abs() < 1e-4, "z3 stop, got {}", text_halo_color.value_at(3).a);
        // z1 is a TRUE interior sample: linear between (0, 0.75) and
        // (3, 1.0) puts it at 0.75 + 0.25/3 = 0.8333. A clamp-to-endpoint
        // bug would read 0.75 here.
        let interior = text_halo_color.value_at(1).a;
        assert!((interior - (0.75 + 0.25 / 3.0)).abs() < 1e-3, "z1 must be a true interior sample, got {interior}");
        assert!(text_halo_color.value_at(0).r > 0.99, "the ramp is white-to-white; only alpha varies");
    }

    #[test]
    fn expands_a_match_shaped_text_halo_color_into_one_filtered_case_each_like_text_color() {
        // Routing `text-halo-color` through `compile_zoom_value` has to
        // mirror `text-color`'s convention for a property-varying result --
        // one `StyleLayer::Symbol` per case, with that case's filter ANDed
        // in -- rather than inventing a second convention (picking a case,
        // or refusing). This `match` shape is VERBATIM from `road-label`'s
        // paint in `examples/mapbox-streets-style.json`
        // (`ferry-aerialway-label` has the same shape on `class`); both of
        // those layers stay skipped for an unrelated reason (they are
        // `symbol-placement: line`), so this exercises the shape directly.
        let layer = symbol_layer_with(serde_json::json!({
            "paint": {
                "text-halo-color": [
                    "match", ["get", "class"],
                    ["motorway", "trunk"], "hsla(20, 25%, 100%, 0.75)",
                    "hsl(20, 25%, 100%)"
                ]
            }
        }));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("a match halo color must compile");
        assert_eq!(compiled.len(), 2, "one Symbol layer per match case (the named group + the default)");
        for layer in &compiled {
            let StyleLayer::Symbol { filter, .. } = layer else { panic!("expected Symbol layers") };
            assert!(!matches!(filter, Filter::Any), "each case must carry its own filter, got {filter:?}");
        }
    }

    #[test]
    fn accepts_a_fractional_minzoom_by_flooring_it_and_a_fractional_maxzoom_by_ceiling_it() {
        // `continent-label`'s real `"minzoom"` is `0.75` -- the ONLY
        // fractional zoom bound anywhere in the vendored style (every layer
        // checked). `zoom_bound` used to hard-error on it ("minzoom" must be
        // a whole number between 0 and 30, got 0.75). Flooring the min and
        // ceiling the max is the conservative direction: it WIDENS the
        // visible zoom range rather than hiding the layer at a zoom the
        // style says it should be drawn at.
        let floored = symbol_layer_with(serde_json::json!({"minzoom": 0.75, "maxzoom": 3}));
        let compiled = compile_symbol_layer(&floored, Scope::root()).expect("a fractional minzoom must compile");
        let StyleLayer::Symbol { min_zoom, max_zoom, .. } = &compiled[0] else {
            panic!("expected a Symbol layer");
        };
        assert_eq!(*min_zoom, Some(0), "0.75 must floor to 0, not round to 1 (which would hide the layer at z0)");
        assert_eq!(*max_zoom, Some(3));

        // No real fractional `maxzoom` exists in either vendored style, so
        // the ceiling direction is pinned synthetically -- it is the same
        // "widen, don't narrow" rule applied to the other end.
        let ceiled = symbol_layer_with(serde_json::json!({"maxzoom": 12.5}));
        let compiled = compile_symbol_layer(&ceiled, Scope::root()).expect("a fractional maxzoom must compile");
        let StyleLayer::Symbol { max_zoom, .. } = &compiled[0] else { panic!("expected a Symbol layer") };
        assert_eq!(*max_zoom, Some(13), "12.5 must ceil to 13, not truncate to 12");

        // Out-of-range bounds are still hard errors -- widening the
        // fractional case must not quietly widen the range check too.
        assert!(compile_symbol_layer(&symbol_layer_with(serde_json::json!({"minzoom": -1.0})), Scope::root()).is_err());
        assert!(compile_symbol_layer(&symbol_layer_with(serde_json::json!({"maxzoom": 31.5})), Scope::root()).is_err());
    }

    /// A single numeric feature tag, the shape `DataStepShape` reads.
    /// Splits `take_degradations`'s output into the two kinds this crate
    /// records, which are NOT the same failure and must not be asserted on
    /// together.
    ///
    /// The FOLDER half (returned second) is a paint property the constant
    /// folder could not reduce, which fell back to a literal -- for this
    /// fixture that must stay at zero, and
    /// `the_vendored_mapbox_standard_dark_fixture_compiles_with_no_folder_degradations`
    /// is what says so.
    ///
    /// The CAPABILITY half (returned first) is a construct this crate has no
    /// representation for, dropped so its layer survives: the `icon-image`
    /// `["image", name, {...}]` options (`note_dropped_image_options`), a
    /// `text-offset` shape `LayoutValue` cannot hold, and a `let` filter
    /// whose inlined body `Filter` cannot express (`compile_let_filter`).
    /// Each of those is a named, documented gap with a visible consequence,
    /// and each is pinned by count where it is expected.
    ///
    /// Discriminated on the CAPABILITY notices' own wording, which is the
    /// closed set (three call sites, all in this crate's own compiler:
    /// `note_dropped_image_options`, `compile_symbol_layer`'s
    /// `degrade_text_offset`, and `filter::compile_let_filter`). Everything
    /// else is a folder notice -- the folder has many more phrasings than
    /// these, "could not be folded", "folded into", "would not fold" and
    /// "cannot both be folded" among them, and matching on THAT side would
    /// silently reclassify a folder failure as an accepted gap the moment a
    /// new one was worded differently.
    fn split_degradations(all: Vec<String>) -> (Vec<String>, Vec<String>) {
        all.into_iter().partition(|notice| {
            notice.starts_with("icon-image ") || notice.starts_with("filter ") || notice.contains(": text-offset ")
        })
    }

    /// The exact capability gaps the vendored Standard Dark document hits,
    /// by kind, measured and then pinned. All three are properties of the
    /// DOCUMENT, so the same three counts must come back from every config
    /// combination -- which is why this is one helper called from both the
    /// pinned-config run and the twelve-combination loop.
    ///
    ///  - 20 `["image", name, {"params": ...}]` option drops: the nine
    ///    one-way-arrow layers and `crosswalks` each write two, a small
    ///    sprite below z18 and a large one above.
    ///  - 0 `text-offset`: this was 1 until `LayoutValue::ZoomOfDataRamp`
    ///    landed. The one drop was `poi-label`'s -- a zoom step whose
    ///    brackets are `sizerank`-driven ramps -- and it is exactly what
    ///    put every POI name on top of its own icon, since the label had
    ///    nothing left to move it once `text-anchor` had degraded to
    ///    `center` in the same breath. Both compile now, and the zero here
    ///    is the standing proof of it.
    ///  - 1 `let` filter: `poi-label`'s again, whose inlined body is a
    ///    comparison with a per-feature AND per-zoom threshold.
    ///
    /// If any of these moves, a layer either stopped reaching a construct or
    /// started dropping more than it used to; both deserve a failing test.
    fn assert_capability_degradations(capability: &[String]) {
        let images = capability.iter().filter(|n| n.starts_with("icon-image ")).count();
        let text_offsets = capability.iter().filter(|n| n.contains(": text-offset ")).count();
        let filters = capability.iter().filter(|n| n.starts_with("filter ")).count();
        assert_eq!(
            (images, text_offsets, filters, capability.len()),
            (20, 0, 1, 21),
            "the fixture's dropped constructs must stay exactly accounted for:\n{}",
            capability.join("\n")
        );
    }

    fn numeric_tag(key: &str, value: f64) -> HashMap<String, TagValue> {
        HashMap::from([(key.to_string(), TagValue::Number(value))])
    }

    fn string_tag(key: &str, value: &str) -> HashMap<String, TagValue> {
        HashMap::from([(key.to_string(), TagValue::String(value.to_string()))])
    }

    #[test]
    fn compiles_the_real_state_label_cubic_bezier_over_symbolrank_step_stops() {
        // VERBATIM from `state-label`'s `text-size` in
        // `examples/mapbox-streets-style.json`. Two blockers in one
        // expression: a `cubic-bezier` interpolation type, and stop OUTPUTS
        // that are `step` expressions on a feature property rather than
        // numeric leaves. The result is a `ZoomOfDataStep` -- "at each
        // zoom, a step on `symbolrank`" -- which is exactly the shape that
        // variant already exists to carry.
        let json = serde_json::json!([
            "interpolate", ["cubic-bezier", 0.85, 0.7, 0.65, 1], ["zoom"],
            4, ["step", ["get", "symbolrank"], 9, 6, 8, 7, 7],
            9, ["step", ["get", "symbolrank"], 21, 6, 16, 7, 14]
        ]);
        let value = compile_layout_value(&json).expect("step-shaped stops must compile");
        assert!(matches!(value, crate::style::LayoutValue::ZoomOfDataStep(_)), "got {value:?}");

        // A feature below the first threshold falls back to each zoom's
        // `base` bucket; symbolrank 6 and 7 pick the two explicit buckets.
        let base = |z: u8| value.resolve(&numeric_tag("symbolrank", 0.0), z);
        let rank6 = |z: u8| value.resolve(&numeric_tag("symbolrank", 6.0), z);
        let rank7 = |z: u8| value.resolve(&numeric_tag("symbolrank", 7.0), z);

        // Both declared zoom stops come back exactly, bucket for bucket.
        assert!((base(4) - 9.0).abs() < 1e-3, "z4 base, got {}", base(4));
        assert!((rank6(4) - 8.0).abs() < 1e-3);
        assert!((rank7(4) - 7.0).abs() < 1e-3);
        assert!((base(9) - 21.0).abs() < 1e-3, "z9 base, got {}", base(9));
        assert!((rank6(9) - 16.0).abs() < 1e-3);
        assert!((rank7(9) - 14.0).abs() < 1e-3);

        // TRUE interior samples, per bucket, computed independently from
        // the bezier easing (u = (z-4)/5, eased, then lerped between the
        // two stops' matching buckets). If the interpolation were clamping
        // to a bracketing stop these would read 9/8/7 instead.
        assert!((base(6) - 13.515958).abs() < 1e-3, "z6 base, got {}", base(6));
        assert!((rank6(6) - 11.010639).abs() < 1e-3, "z6 rank6, got {}", rank6(6));
        assert!((rank7(6) - 9.634309).abs() < 1e-3, "z6 rank7, got {}", rank7(6));
        assert!((base(7) - 16.528863).abs() < 1e-3, "z7 base, got {}", base(7));
        assert!((rank7(8) - 13.488971).abs() < 1e-3, "z8 rank7, got {}", rank7(8));

        // The bucket ORDER must survive: at every zoom, a higher symbolrank
        // (a less important place) is drawn smaller.
        for z in 4..=9 {
            assert!(base(z) > rank6(z) && rank6(z) > rank7(z), "bucket order broke at z{z}");
        }
    }

    #[test]
    fn compiles_the_real_country_label_cubic_bezier_over_symbolrank_step_stops() {
        // VERBATIM from `country-label`'s `text-size`. Same shape as
        // `state-label`'s, with a different bezier and different buckets --
        // included because it is a second real instance, and because
        // `country-label` needs the Stage-B1 halo-color fix as well, so it
        // is the layer that proves the two changes compose.
        let json = serde_json::json!([
            "interpolate", ["cubic-bezier", 0.2, 0, 0.7, 1], ["zoom"],
            1, ["step", ["get", "symbolrank"], 11, 4, 9, 5, 8],
            9, ["step", ["get", "symbolrank"], 22, 4, 19, 5, 17]
        ]);
        let value = compile_layout_value(&json).expect("step-shaped stops must compile");
        let base = |z: u8| value.resolve(&numeric_tag("symbolrank", 0.0), z);
        let rank4 = |z: u8| value.resolve(&numeric_tag("symbolrank", 4.0), z);
        let rank5 = |z: u8| value.resolve(&numeric_tag("symbolrank", 5.0), z);

        assert!((base(1) - 11.0).abs() < 1e-3);
        assert!((rank5(1) - 8.0).abs() < 1e-3);
        assert!((base(9) - 22.0).abs() < 1e-3);
        assert!((rank5(9) - 17.0).abs() < 1e-3);

        // Interior samples, independently computed. z5 is the curve's
        // midpoint: the bezier eases u = 0.5 to 0.549731, decisively NOT
        // the linear 0.5 -- which is what proves the declared easing type
        // is honoured rather than quietly degraded to linear.
        assert!((base(5) - 17.047038).abs() < 1e-3, "z5 base, got {}", base(5));
        assert!((rank4(5) - 14.497307).abs() < 1e-3, "z5 rank4, got {}", rank4(5));
        assert!((rank5(5) - 12.947576).abs() < 1e-3, "z5 rank5, got {}", rank5(5));
        assert!((base(3) - 13.410521).abs() < 1e-3, "z3 base, got {}", base(3));
        assert!((rank4(7) - 17.48494).abs() < 1e-3, "z7 rank4, got {}", rank4(7));
    }

    /// THE justification for Stage D's threshold-union refinement, proved
    /// directly rather than argued in a comment.
    ///
    /// The whole relaxation rests on one claim: re-expressing a `step` on a
    /// SUPERSET of its own thresholds cannot change what it returns for any
    /// input, because each added threshold merely repeats the value already
    /// in effect at that point. If that claim were false the refinement
    /// would be silently rewriting real label sizes, so this test sweeps a
    /// real stop expression over property values below, exactly at, between
    /// and above every threshold -- of BOTH the original and the added ones,
    /// since an added threshold is precisely where a bug would show -- and
    /// demands bit-identical results.
    #[test]
    fn refining_a_data_step_onto_a_superset_of_its_thresholds_resolves_identically_for_every_input() {
        use crate::style::DataStepShape;

        // VERBATIM the z6 stop of `settlement-major-label`'s real
        // `text-size`: ["step", ["get", "symbolrank"], 18, 6, 16, 7, 14].
        // Its own thresholds are [6, 7]; the union its siblings force it
        // onto is [6, 7, 9, 10, 12, 15].
        let original =
            DataStepShape { key: "symbolrank".to_string(), base: 18.0, stops: vec![(6.0, 16.0), (7.0, 14.0)] };
        let union = [6.0, 7.0, 9.0, 10.0, 12.0, 15.0];
        let refined = refine_data_step_onto_thresholds(&original, &union);

        // Refinement adds buckets, never removes or reorders them, and
        // never touches the base (the "below every threshold" value).
        assert_eq!(refined.key, original.key);
        assert_eq!(refined.base, original.base);
        assert_eq!(
            refined.stops,
            vec![(6.0, 16.0), (7.0, 14.0), (9.0, 14.0), (10.0, 14.0), (12.0, 14.0), (15.0, 14.0)],
            "each ADDED threshold must repeat the value already in effect there (14, from >=7)"
        );

        // The sweep. For every threshold in the union -- so every point
        // where the refined shape has a boundary the original does not --
        // probe just below it, exactly on it, and just above it, plus a
        // point between consecutive thresholds and points beyond both ends.
        let mut probes: Vec<f64> = vec![f64::MIN, -1.0, 0.0, 5.5, 6.5, 8.0, 11.0, 13.5, 20.0, 1e9];
        for t in union {
            probes.extend([t - 1e-6, t, t + 1e-6, t - 0.5, t + 0.5]);
        }
        for probe in probes {
            let tags = numeric_tag("symbolrank", probe);
            assert_eq!(
                refined.resolve(&tags),
                original.resolve(&tags),
                "refinement changed the value at symbolrank {probe}"
            );
        }

        // A feature with no `symbolrank` tag at all (or a non-numeric one)
        // falls back to `base` in both shapes -- the one path that does not
        // go through the thresholds, so it needs its own probe.
        let untagged = HashMap::new();
        assert_eq!(refined.resolve(&untagged), original.resolve(&untagged));
        let stringly = HashMap::from([("symbolrank".to_string(), TagValue::String("9".into()))]);
        assert_eq!(refined.resolve(&stringly), original.resolve(&stringly));
    }

    #[test]
    fn compiles_the_real_settlement_major_label_text_size_whose_stops_use_different_symbolrank_buckets() {
        // VERBATIM from `settlement-major-label`'s `text-size` in
        // `examples/mapbox-streets-style.json`. The blocker Stage D lifts:
        // the four zoom stops step on `symbolrank` with FOUR DIFFERENT
        // threshold sets -- [6], [6,7], [9,10], [9,12,15] -- so there was no
        // "corresponding bucket" to interpolate until the compiler put them
        // all on the union [6,7,9,10,12,15] first.
        let json = serde_json::json!([
            "interpolate", ["cubic-bezier", 0.2, 0, 0.9, 1], ["zoom"],
            3, ["step", ["get", "symbolrank"], 13, 6, 11],
            6, ["step", ["get", "symbolrank"], 18, 6, 16, 7, 14],
            8, ["step", ["get", "symbolrank"], 20, 9, 16, 10, 14],
            15, ["step", ["get", "symbolrank"], 24, 9, 20, 12, 16, 15, 14]
        ]);
        let value = compile_layout_value(&json).expect("mismatched bucket thresholds must now unify");
        assert!(matches!(value, crate::style::LayoutValue::ZoomOfDataStep(_)), "got {value:?}");
        let at = |rank: f64, z: u8| value.resolve(&numeric_tag("symbolrank", rank), z);

        // Every declared stop must come back EXACTLY, bucket for bucket --
        // including the buckets that stop never declared. At z8 a
        // symbolrank-6 city is below that stop's first threshold (9), so it
        // takes the base 20; the refinement has to reproduce that, not
        // inherit 16 from the z6 stop's ">=6" bucket.
        assert!((at(0.0, 3) - 13.0).abs() < 1e-3, "z3 base, got {}", at(0.0, 3));
        assert!((at(6.0, 3) - 11.0).abs() < 1e-3);
        assert!((at(15.0, 3) - 11.0).abs() < 1e-3, "z3 has ONE bucket: every rank >=6 is 11");
        assert!((at(0.0, 6) - 18.0).abs() < 1e-3);
        assert!((at(6.0, 6) - 16.0).abs() < 1e-3);
        assert!((at(7.0, 6) - 14.0).abs() < 1e-3);
        assert!((at(12.0, 6) - 14.0).abs() < 1e-3, "z6's top bucket is >=7, so rank 12 is 14 too");
        assert!((at(6.0, 8) - 20.0).abs() < 1e-3, "z8 rank6 is BELOW its first threshold -> base 20");
        assert!((at(9.0, 8) - 16.0).abs() < 1e-3);
        assert!((at(10.0, 8) - 14.0).abs() < 1e-3);
        assert!((at(0.0, 15) - 24.0).abs() < 1e-3);
        assert!((at(9.0, 15) - 20.0).abs() < 1e-3);
        assert!((at(12.0, 15) - 16.0).abs() < 1e-3);
        assert!((at(15.0, 15) - 14.0).abs() < 1e-3);

        // TRUE INTERIOR samples, independently computed from the bezier
        // [0.2,0,0.9,1] against the REFINED stops. These are the assertions
        // a clamp bug cannot pass: z7 sits between two stops whose declared
        // threshold sets are disjoint apart from nothing at all ([6,7] vs.
        // [9,10]), so every one of these numbers depends on the union
        // having been built correctly.
        assert!((at(0.0, 7) - 18.912104).abs() < 1e-3, "z7 base, got {}", at(0.0, 7));
        assert!((at(6.0, 7) - 17.824207).abs() < 1e-3, "z7 rank6, got {}", at(6.0, 7));
        assert!((at(7.0, 7) - 16.736313).abs() < 1e-3, "z7 rank7, got {}", at(7.0, 7));
        assert!((at(9.0, 7) - 14.912104).abs() < 1e-3, "z7 rank9, got {}", at(9.0, 7));
        assert!((at(10.0, 7) - 14.0).abs() < 1e-3, "z7 rank10 is 14 at BOTH ends, so it must stay 14");
        assert!((at(0.0, 4) - 14.336437).abs() < 1e-3, "z4 base, got {}", at(0.0, 4));
        assert!((at(6.0, 5) - 14.265345).abs() < 1e-3, "z5 rank6, got {}", at(6.0, 5));
        assert!((at(10.0, 11) - 16.241375).abs() < 1e-3, "z11 rank10, got {}", at(10.0, 11));
        assert!((at(12.0, 12) - 15.080057).abs() < 1e-3, "z12 rank12, got {}", at(12.0, 12));

        // The bucket ORDER must survive every zoom: a higher symbolrank is
        // a less important place and is never drawn larger.
        for z in 3..=15 {
            for pair in [(0.0, 6.0), (6.0, 7.0), (7.0, 9.0), (9.0, 10.0), (10.0, 12.0), (12.0, 15.0)] {
                assert!(at(pair.0, z) >= at(pair.1, z), "bucket order broke at z{z} between ranks {pair:?}");
            }
        }
    }

    #[test]
    fn compiles_the_real_settlement_minor_label_text_size_whose_stops_use_different_symbolrank_buckets() {
        // VERBATIM from `settlement-minor-label`'s `text-size`. Included as
        // a second real instance because its threshold sets are NESTED
        // ([9] c [9,12] c [9,12,15]) rather than the major layer's
        // partly-disjoint ones -- the union is just the largest set, which
        // exercises the "some stops are already fully refined" path.
        let json = serde_json::json!([
            "interpolate", ["cubic-bezier", 0.2, 0, 0.9, 1], ["zoom"],
            3, ["step", ["get", "symbolrank"], 11, 9, 10],
            6, ["step", ["get", "symbolrank"], 14, 9, 12, 12, 10],
            8, ["step", ["get", "symbolrank"], 16, 9, 14, 12, 12, 15, 10],
            13, ["step", ["get", "symbolrank"], 22, 9, 20, 12, 16, 15, 14]
        ]);
        let value = compile_layout_value(&json).expect("nested bucket thresholds must unify too");
        let at = |rank: f64, z: u8| value.resolve(&numeric_tag("symbolrank", rank), z);

        // Declared stops, exactly -- including the buckets the z3 and z6
        // stops never spell out (rank 12 and 15 at z3 must read 10, the
        // value its single ">=9" bucket already had in effect there).
        assert!((at(0.0, 3) - 11.0).abs() < 1e-3);
        assert!((at(9.0, 3) - 10.0).abs() < 1e-3);
        assert!((at(15.0, 3) - 10.0).abs() < 1e-3, "z3's only bucket is >=9");
        assert!((at(12.0, 6) - 10.0).abs() < 1e-3);
        assert!((at(15.0, 6) - 10.0).abs() < 1e-3, "z6's top bucket is >=12");
        assert!((at(15.0, 8) - 10.0).abs() < 1e-3);
        assert!((at(9.0, 13) - 20.0).abs() < 1e-3);
        assert!((at(15.0, 13) - 14.0).abs() < 1e-3);

        // Interior samples, independently computed. z4/z5 interpolate
        // between the one-bucket z3 stop and the two-bucket z6 one, so
        // rank 12 and 15 must track the ADDED buckets, not the base.
        assert!((at(0.0, 4) - 11.801863).abs() < 1e-3, "z4 base, got {}", at(0.0, 4));
        assert!((at(9.0, 4) - 10.534575).abs() < 1e-3, "z4 rank9, got {}", at(9.0, 4));
        assert!((at(12.0, 5) - 10.0).abs() < 1e-3, "z5 rank12 is 10 at both ends, got {}", at(12.0, 5));
        assert!((at(12.0, 7) - 10.912104).abs() < 1e-3, "z7 rank12, got {}", at(12.0, 7));
        assert!((at(15.0, 7) - 10.0).abs() < 1e-3, "z7 rank15, got {}", at(15.0, 7));
        assert!((at(0.0, 10) - 18.04693).abs() < 1e-3, "z10 base, got {}", at(0.0, 10));
        assert!((at(9.0, 10) - 16.04693).abs() < 1e-3, "z10 rank9, got {}", at(9.0, 10));
        assert!((at(12.0, 11) - 14.295501).abs() < 1e-3, "z11 rank12, got {}", at(12.0, 11));
        assert!((at(15.0, 12) - 13.239301).abs() < 1e-3, "z12 rank15, got {}", at(15.0, 12));
    }

    #[test]
    fn compiles_the_real_settlement_major_and_minor_label_symbol_layers_end_to_end() {
        // The layers this whole stage exists for: Mapbox Streets' CITY
        // labels. They were soft-skipped in their entirety -- first on
        // `icon-image`, then (Stage C) on this `text-size` -- so the map
        // drew no city names at all.
        let style = or_skip!(mapbox_streets());
        for id in ["settlement-major-label", "settlement-minor-label"] {
            let compiled: Vec<_> = style
                .layers
                .iter()
                .filter(|l| matches!(l, StyleLayer::Symbol { id: got, .. } if got == id))
                .collect();
            assert_eq!(compiled.len(), 1, "{id} must now compile, into exactly one Symbol layer");
            let StyleLayer::Symbol { text_size, text_field, icon_image, .. } = compiled[0] else {
                unreachable!("filtered to Symbol above")
            };
            assert!(text_field.is_some(), "{id} must carry its text");
            // Stage C taught the compiler this layer's data-driven
            // `icon-image`, so the recovered layer draws its dots too --
            // it is NOT the text-only degradation fallback.
            assert!(icon_image.is_some(), "{id} must carry its icon, not degrade to text-only");
            // One interior sample per layer, so the end-to-end path is
            // pinned to the same numbers the unit tests above computed --
            // proving the vendored JSON really is the expression they quote.
            let rank = |r: f64, z: u8| text_size.resolve(&numeric_tag("symbolrank", r), z);
            if id == "settlement-major-label" {
                assert!((rank(6.0, 7) - 17.824207).abs() < 1e-3, "got {}", rank(6.0, 7));
            } else {
                assert!((rank(12.0, 7) - 10.912104).abs() < 1e-3, "got {}", rank(12.0, 7));
            }
        }
    }

    #[test]
    fn rejects_step_shaped_interpolate_stops_that_step_on_different_properties_but_unifies_their_thresholds() {
        // What survives Stage D unchanged: stops that step on DIFFERENT
        // properties. There is genuinely no correspondence between a
        // `symbolrank` bucket and a `sizerank` one -- pairing them up would
        // be a guess -- so this stays a clear Err, i.e. a soft per-layer
        // skip.
        let different_key = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            4, ["step", ["get", "symbolrank"], 9, 6, 8],
            9, ["step", ["get", "sizerank"], 21, 6, 16]
        ]);
        let err = compile_layout_value(&different_key).expect_err("mismatched keys must be refused");
        assert!(err.to_string().contains("symbolrank") && err.to_string().contains("sizerank"), "got {err}");

        // What Stage D changes: stops on the SAME property whose thresholds
        // differ are no longer refused, because refining both onto the
        // union of their thresholds is lossless (see
        // `refining_a_data_step_onto_a_superset_of_its_thresholds_resolves_identically_for_every_input`).
        // Stage B refused both of these.
        let different_thresholds = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            4, ["step", ["get", "symbolrank"], 9, 6, 8, 7, 7],
            9, ["step", ["get", "symbolrank"], 21, 6, 16]
        ]);
        let value = compile_layout_value(&different_thresholds).expect("differing bucket counts now unify");
        let at = |rank: f64, z: u8| value.resolve(&numeric_tag("symbolrank", rank), z);
        // The z9 stop has no ">=7" bucket, so rank 7 there is whatever its
        // ">=6" bucket says (16) -- NOT the z4 stop's 7 carried forward.
        assert!((at(7.0, 9) - 16.0).abs() < 1e-3, "got {}", at(7.0, 9));
        assert!((at(7.0, 4) - 7.0).abs() < 1e-3);
        // Linear midpoint z6.5 is not an integer zoom; z7 is 0.6 of the way.
        assert!((at(7.0, 7) - (7.0 + 0.6 * 9.0)).abs() < 1e-3, "got {}", at(7.0, 7));

        // Disjoint threshold sets unify the same way: the union is [5, 7],
        // and each stop repeats the value already in effect at the
        // threshold it lacks.
        let disjoint = serde_json::json!([
            "interpolate", ["linear"], ["zoom"],
            4, ["step", ["get", "symbolrank"], 9, 7, 7],
            9, ["step", ["get", "symbolrank"], 21, 5, 16]
        ]);
        let value = compile_layout_value(&disjoint).expect("disjoint bucket thresholds now unify");
        let at = |rank: f64, z: u8| value.resolve(&numeric_tag("symbolrank", rank), z);
        assert!((at(5.0, 4) - 9.0).abs() < 1e-3, "z4 has no >=5 bucket, so rank 5 takes its base 9");
        assert!((at(5.0, 9) - 16.0).abs() < 1e-3);
        assert!((at(7.0, 4) - 7.0).abs() < 1e-3);
        assert!((at(7.0, 9) - 16.0).abs() < 1e-3, "z9's >=5 bucket still covers rank 7");
    }

    #[test]
    fn compiles_the_real_settlement_subdivision_label_match_shaped_stops_and_interpolates_them() {
        // VERBATIM from `settlement-subdivision-label`'s `text-size`. Its
        // `cubic-bezier` type already compiled; its OTHER blocker was that
        // its stop outputs are `match` expressions on the STRING property
        // `type` ("suburb" vs. everything else), which `MatchStepShape`/
        // `LayoutValue::ZoomOfMatchStep` now represent directly (see
        // `compile_match_step_stops`).
        let json = serde_json::json!([
            "interpolate", ["cubic-bezier", 0.5, 0, 1, 1], ["zoom"],
            11, ["match", ["get", "type"], "suburb", 11, 10.5],
            15, ["match", ["get", "type"], "suburb", 15, 14]
        ]);
        let value = compile_layout_value(&json).expect("match-shaped stops on a cubic-bezier curve now compile");
        let suburb = string_tag("type", "suburb");
        let hamlet = string_tag("type", "hamlet"); // matches no arm, takes the default

        // At the two declared stops, exactly.
        assert!((value.resolve(&suburb, 11) - 11.0).abs() < 1e-3);
        assert!((value.resolve(&hamlet, 11) - 10.5).abs() < 1e-3);
        assert!((value.resolve(&suburb, 15) - 15.0).abs() < 1e-3);
        assert!((value.resolve(&hamlet, 15) - 14.0).abs() < 1e-3);

        // At an intermediate zoom, mixed along the DECLARED cubic-bezier
        // curve -- not a linear fallback. Computed via the same
        // `expression::Interpolation` the production code mixes with
        // (`match_step_at`), rather than hand-deriving the bezier's `t`
        // here; this test's job is the ARM mixing, not re-proving the curve
        // math `compiles_the_real_state_label_cubic_bezier_over_symbolrank_step_stops`
        // already pins.
        let curve = expression::Interpolation::parse(&serde_json::json!(["cubic-bezier", 0.5, 0, 1, 1]), &json)
            .expect("the curve parses");
        let t = curve.t_at(13.0, 11.0, 15.0) as f32;
        assert!((0.0..=1.0).contains(&t), "sanity: t must stay in range, got {t}");
        let expected_suburb = 11.0 + (15.0 - 11.0) * t;
        let expected_hamlet = 10.5 + (14.0 - 10.5) * t;
        assert!(
            (value.resolve(&suburb, 13) - expected_suburb).abs() < 1e-3,
            "got {} expected {}",
            value.resolve(&suburb, 13),
            expected_suburb
        );
        assert!(
            (value.resolve(&hamlet, 13) - expected_hamlet).abs() < 1e-3,
            "got {} expected {}",
            value.resolve(&hamlet, 13),
            expected_hamlet
        );
    }

    #[test]
    fn compiles_the_real_state_label_and_country_label_symbol_layers_end_to_end() {
        // End-to-end: both REAL vendored layers were soft-skipped in their
        // entirety on "unsupported interpolation type \"cubic-bezier\"".
        let style = or_skip!(mapbox_streets());

        let state: Vec<_> = style
            .layers
            .iter()
            .filter_map(|l| match l {
                StyleLayer::Symbol { id, text_size, .. } if id == "state-label" => Some(text_size),
                _ => None,
            })
            .collect();
        assert_eq!(state.len(), 1, "state-label must now compile (into exactly one Symbol layer)");
        assert!((state[0].resolve(&numeric_tag("symbolrank", 6.0), 4) - 8.0).abs() < 1e-3);
        assert!((state[0].resolve(&numeric_tag("symbolrank", 6.0), 6) - 11.010639).abs() < 1e-3);

        let country: Vec<_> = style
            .layers
            .iter()
            .filter_map(|l| match l {
                StyleLayer::Symbol { id, text_size, text_halo_color, .. } if id == "country-label" => {
                    Some((text_size, text_halo_color))
                }
                _ => None,
            })
            .collect();
        assert_eq!(country.len(), 1, "country-label must now compile");
        let (text_size, text_halo_color) = country[0];
        assert!((text_size.resolve(&numeric_tag("symbolrank", 5.0), 5) - 12.947576).abs() < 1e-3);
        // country-label needed BOTH stages: its halo color is the Stage-B1
        // `interpolate` alpha ramp (2 -> 0.75, 3 -> 1.0).
        assert!((text_halo_color.value_at(2).a - 0.75).abs() < 1e-4);
        assert!((text_halo_color.value_at(3).a - 1.0).abs() < 1e-4);
    }

    #[test]
    fn compiles_the_real_continent_label_symbol_layer_end_to_end() {
        // End-to-end: `continent-label` had THREE independent blockers.
        // Stage A fixed the first (its `text-size` interpolate); this
        // change fixes the other two -- the `interpolate`-shaped
        // `text-halo-color` and the fractional `"minzoom": 0.75` -- so the
        // real layer now compiles out of the vendored style for the first
        // time.
        let style = or_skip!(mapbox_streets());
        let compiled: Vec<_> = style
            .layers
            .iter()
            .filter(|l| matches!(l, StyleLayer::Symbol { id, .. } if id == "continent-label"))
            .collect();
        assert_eq!(compiled.len(), 1, "continent-label must now compile (into exactly one Symbol layer)");
        let StyleLayer::Symbol { min_zoom, max_zoom, text_size, text_halo_color, .. } = compiled[0] else {
            panic!("expected a Symbol layer");
        };
        assert_eq!(*min_zoom, Some(0), "the real 0.75 minzoom floors to 0");
        assert_eq!(*max_zoom, Some(3));
        // Its real text-size curve (["exponential", 0.5], 0 -> 10, 2.5 -> 15).
        assert!((text_size.resolve(&HashMap::new(), 0) - 10.0).abs() < 1e-4);
        assert!(text_size.resolve(&HashMap::new(), 1) > 12.0, "a base-0.5 curve is front-loaded");
        // Its real halo-color alpha ramp, sampled strictly inside.
        assert!((text_halo_color.value_at(0).a - 0.75).abs() < 1e-4);
        let interior = text_halo_color.value_at(1).a;
        assert!(interior > 0.75 && interior < 1.0, "z1 must be a true interior sample, got {interior}");
    }

    /// An `icon-image` shape the compiler doesn't understand used to fail
    /// `compile_symbol_layer` outright, which soft-skipped the WHOLE layer
    /// -- throwing away its text as well. For a label layer that is the
    /// wrong trade: the text is the point of the layer and the icon is
    /// decoration, so a layer whose text-field compiles must still compile,
    /// as text-only. `["concat", ["upcase", ...], ...]` stays out of scope
    /// (only a literal, `["get", k]` and `["to-string", ["get", k]]` are
    /// supported concat parts), so it stands in here for "any icon-image the
    /// compiler doesn't understand".
    #[test]
    fn an_unsupported_icon_image_degrades_the_layer_to_text_only_instead_of_skipping_it() {
        let json = serde_json::json!({
            "id": "road-exit-shield",
            "type": "symbol",
            "source-layer": "motorway_junction",
            "layout": {
                "text-field": ["get", "ref"],
                "text-size": 9,
                "icon-image": ["concat", ["upcase", ["get", "shield"]], "-", "3"]
            },
            "paint": {"text-color": "hsl(0, 0%, 100%)"}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("an unsupported icon must not sink the layer");
        assert_eq!(layers.len(), 1);
        match &layers[0] {
            StyleLayer::Symbol { id, text_field, icon_image, .. } => {
                assert_eq!(id, "road-exit-shield");
                assert!(text_field.is_some(), "the text-field is the point of the layer and must survive");
                assert!(icon_image.is_none(), "the unsupported icon must be dropped, not guessed at");
            }
            other => panic!("expected StyleLayer::Symbol, got {other:?}"),
        }
    }

    /// The other half of the trade: dropping an unsupported icon is only
    /// acceptable when something else in the layer still draws. A layer
    /// with an unsupported `icon-image` and NO usable text-field would
    /// render literally nothing, so it must keep skipping exactly as it did
    /// before -- emitting an empty layer would be worse than useless, it
    /// would cost per-frame work for no pixels.
    #[test]
    fn an_unsupported_icon_image_with_no_text_field_still_skips_the_layer() {
        let json = serde_json::json!({
            "id": "icon-only-shield",
            "type": "symbol",
            "source-layer": "road",
            "layout": {
                "icon-image": ["concat", ["upcase", ["get", "shield"]], "-", "3"]
            }
        });
        assert!(
            compile_symbol_layer(&json, Scope::root()).is_err(),
            "a layer with neither text nor icon draws nothing and must not be emitted"
        );
    }

    /// End-to-end over the real vendored style: `road-exit-shield`'s
    /// `icon-image` is a bare `["concat", "motorway-exit-", ["to-string",
    /// ["get", "reflen"]]]`, so it now resolves to the motorway-exit badge
    /// sprite alongside its exit-number text.
    #[test]
    fn the_real_road_exit_shield_layer_now_draws_its_badge() {
        let style = or_skip!(mapbox_streets());
        let compiled: Vec<_> = style
            .layers
            .iter()
            .filter(|l| matches!(l, StyleLayer::Symbol { id, .. } if id == "road-exit-shield"))
            .collect();
        assert_eq!(compiled.len(), 1, "road-exit-shield must compile into exactly one Symbol layer");
        let StyleLayer::Symbol { text_field, icon_image, min_zoom, .. } = compiled[0] else {
            panic!("expected a Symbol layer");
        };
        assert_eq!(*min_zoom, Some(14));
        let mut tags = HashMap::new();
        tags.insert("ref".to_string(), TagValue::String("25A".to_string()));
        tags.insert("reflen".to_string(), TagValue::Number(3.0));
        assert_eq!(text_field.as_ref().unwrap().resolve(&tags).as_deref(), Some("25A"));
        assert_eq!(
            icon_image.as_ref().unwrap().resolve(&tags, 14).as_deref(),
            Some("motorway-exit-3"),
            "its concat-built shield name must now resolve to the exit badge sprite"
        );
    }

    /// Both route-shield layers, over the real vendored style: `Task 3/4`
    /// let `road-number-shield` compile at all, and `Task 2` gave its
    /// `icon-image` a two-generation `case`; this checks every expanded
    /// case of both layers still carries an icon rather than silently
    /// degrading to text-only.
    #[test]
    fn both_mapbox_streets_shield_layers_compile() {
        let style = or_skip!(mapbox_streets());
        for id in ["road-number-shield", "road-exit-shield"] {
            let cases: Vec<_> = style
                .layers
                .iter()
                .filter(|l| matches!(l, StyleLayer::Symbol { id: layer_id, .. } if layer_id == id))
                .collect();
            assert!(!cases.is_empty(), "{id} must compile");
            for case in &cases {
                let StyleLayer::Symbol { icon_image, .. } = case else { panic!("expected a symbol layer") };
                assert!(icon_image.is_some(), "{id} must carry its icon, not degrade to text-only");
            }
        }
    }

    /// The real shield icon, resolved against tags decoded from a real
    /// Mapbox composite tile over Singapore CBD (12/3229/2033): 11 of its
    /// 19 shield-bearing road features carry exactly `shield:
    /// "rectangle-blue"`, `reflen: 3` (a NUMBER, not a string) -- AYE, PIE,
    /// CTE and ECP.
    #[test]
    fn the_real_shield_icon_resolves_a_singapore_expressway_sprite_name() {
        let style = or_skip!(mapbox_streets());
        let StyleLayer::Symbol { icon_image, .. } = style
            .layers
            .iter()
            .find(|l| matches!(l, StyleLayer::Symbol { id, .. } if id == "road-number-shield"))
            .expect("road-number-shield must compile")
        else {
            panic!("expected a symbol layer")
        };
        let mut tags = HashMap::new();
        tags.insert("shield".to_string(), TagValue::String("rectangle-blue".to_string()));
        tags.insert("reflen".to_string(), TagValue::Number(3.0));
        assert_eq!(
            icon_image.as_ref().expect("icon").resolve(&tags, 12).as_deref(),
            Some("rectangle-blue-3")
        );
    }

    /// `road-number-shield`'s real `layout.symbol-placement` is `["step",
    /// ["zoom"], "point", 11, "line"]`: a shield rides a road's point
    /// feature below z11 and is placed along the line above it.
    #[test]
    fn the_road_number_shield_is_point_placed_below_z11_and_line_placed_above() {
        let style = or_skip!(mapbox_streets());
        let StyleLayer::Symbol { placement, .. } = style
            .layers
            .iter()
            .find(|l| matches!(l, StyleLayer::Symbol { id, .. } if id == "road-number-shield"))
            .expect("road-number-shield must compile")
        else {
            panic!("expected a symbol layer")
        };
        assert_eq!(placement.at(10), SymbolPlacement::Point);
        assert_eq!(placement.at(11), SymbolPlacement::Line);
    }

    /// CRITICAL 1's regression test, run against the REAL compiled
    /// `road-number-shield` layer -- not a hand-built fixture -- through
    /// `match_tile_symbols`, at both of the layer's real placements: point
    /// below z11, line at and above it. `road-number-shield` sets
    /// `text-rotation-alignment: "viewport"` (see the design doc), so its
    /// text must resolve at BOTH placements; before this fix the `Line` arm
    /// never resolved `text_field` at all, so every shield at z11+ rendered
    /// as an empty plate -- exactly the zoom range the feature's success
    /// criterion names. `shield: "rectangle-blue"`/`reflen: 3` is the real
    /// tag pair Singapore's expressways (AYE/PIE/CTE/ECP) carry (see
    /// `the_real_shield_icon_resolves_a_singapore_expressway_sprite_name`
    /// above); `ref` is the separate tag `text-field: ["get", "ref"]` reads.
    #[test]
    fn the_real_shield_text_and_icon_both_resolve_at_point_and_line_placement() {
        let style = Style {
            sprite: None,
            layers: or_skip!(mapbox_streets())
                .layers
                .into_iter()
                .filter(|l| matches!(l, StyleLayer::Symbol { id, .. } if id == "road-number-shield"))
                .collect(),
            sky: Default::default(),
        };

        let mut tags = HashMap::new();
        tags.insert("shield".to_string(), TagValue::String("rectangle-blue".to_string()));
        tags.insert("reflen".to_string(), TagValue::Number(3.0));
        tags.insert("ref".to_string(), TagValue::String("AYE".to_string()));

        // Below z11: point-placed. The filter's own zoom-stepped condition
        // is `["==", ["geometry-type"], "Point"]` below z11 -- a real
        // predicate now (`Filter::GeometryType`, see
        // `filter::compile_comparison`), which this feature's Point geometry
        // satisfies.
        let point_tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature { geometry: Geometry::Point(geo_types::Point::new(100, 100)), tags: tags.clone().into() }],
            }],
        };
        let point_plan = match_tile_symbols(&point_tile, &style, 10, 8.0);
        assert_eq!(point_plan.features.len(), 1, "one shield symbol at z10 (point placement)");
        assert_eq!(point_plan.features[0].icon.as_deref(), Some("rectangle-blue-3"), "z10 icon");
        assert_eq!(point_plan.features[0].text.as_deref(), Some("AYE"), "z10 text -- the bug this test guards against");

        // At and above z11: line-placed. The filter's step at z12 requires
        // `["get", "len"] > 2500`, so the feature needs a `len` tag too.
        let mut line_tags = tags.clone();
        line_tags.insert("len".to_string(), TagValue::Number(3000.0));
        let line_tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: Geometry::LineString(LineString::from(vec![(0, 0), (4000, 0)])),
                    tags: line_tags.into(),
                }],
            }],
        };
        let line_plan = match_tile_symbols(&line_tile, &style, 12, 8.0);
        assert!(!line_plan.features.is_empty(), "at least one shield symbol at z12 (line placement)");
        for feature in &line_plan.features {
            assert_eq!(feature.icon.as_deref(), Some("rectangle-blue-3"), "z12 icon");
            assert_eq!(feature.text.as_deref(), Some("AYE"), "z12 text -- Critical 1: line-placed shields were empty plates");
        }
    }

    /// A line-placed layer with a `text-field` and NO explicit
    /// `text-rotation-alignment` -- the real shape of Mapbox Streets'
    /// road-NAME layers (`road-label` et al) and of Liberty's
    /// `highway-name-*`. Two things have to hold at once: the spec's `auto`
    /// still resolves to `Map` under line placement (the property is
    /// compiled correctly and is there for curve-following placement to
    /// read), AND `match_tile_symbols` produces the label anyway. The pair
    /// is the point: the alignment used to be read as permission to drop
    /// the text, which is what left every road name off the map.
    #[test]
    fn a_line_placed_layer_with_no_explicit_text_rotation_alignment_resolves_to_map_and_still_labels() {
        let json = serde_json::json!({
            "id": "road-name-like",
            "type": "symbol",
            "source-layer": "road",
            "layout": {
                "symbol-placement": "line",
                "text-field": ["get", "name"],
                "icon-image": "arrow"
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("must compile");
        let StyleLayer::Symbol { text_rotation_alignment, .. } = &layers[0] else {
            panic!("expected a symbol layer");
        };
        assert_eq!(
            *text_rotation_alignment,
            RotationAlignment::Map,
            "absent text-rotation-alignment resolves to map under line placement"
        );

        let mut tags = HashMap::new();
        tags.insert("name".to_string(), TagValue::String("Jalan Ampang".to_string()));
        let tile = DecodedTile {
            data_hash: 0,
            layers: vec![Layer {
                name: "road".to_string(),
                features: vec![Feature {
                    geometry: Geometry::LineString(LineString::from(vec![(0, 0), (4000, 0)])),
                    tags: tags.into(),
                }],
            }],
        };
        let plan = match_tile_symbols(&tile, &Style { sprite: None, layers, sky: Default::default() }, 14, 8.0);
        assert!(!plan.features.is_empty(), "the icon must still be placed");
        for feature in &plan.features {
            assert_eq!(feature.text.as_deref(), Some("Jalan Ampang"), "a road-NAME-shaped layer must carry its name");
            assert_eq!(feature.icon.as_deref(), Some("arrow"));
        }
    }

    /// `settlement-major-label`'s and `settlement-minor-label`'s real
    /// `icon-image`, copied VERBATIM out of
    /// `examples/mapbox-streets-style.json` (both layers carry the identical
    /// expression). It is a zoom `step` whose per-zoom value is itself
    /// data-driven: below z8 a `case` on `capital == 2` picking the capital
    /// marker, otherwise a `step` on the numeric `symbolrank` property; from
    /// z8 up, the empty string -- which means NO icon, not an icon literally
    /// named "" (an empty name would be looked up in the sprite atlas and
    /// miss on every feature, every frame).
    #[test]
    fn compiles_the_real_city_label_zoom_step_of_data_driven_icon_names() {
        let json = serde_json::json!(["step", ["zoom"],
            ["case", ["==", ["get", "capital"], 2],
                "border-dot-13",
                ["step", ["get", "symbolrank"], "dot-11", 9, "dot-10", 11, "dot-9"]],
            8, ""]);
        let icon = compile_icon_image(&json).unwrap();

        let capital = numeric_tag("capital", 2.0);
        assert_eq!(icon.resolve(&capital, 0).as_deref(), Some("border-dot-13"));
        assert_eq!(icon.resolve(&capital, 7).as_deref(), Some("border-dot-13"));

        // A non-capital falls through the `case` to the symbolrank step,
        // whose buckets are base (<9), >=9 and >=11.
        assert_eq!(icon.resolve(&numeric_tag("symbolrank", 5.0), 4).as_deref(), Some("dot-11"));
        assert_eq!(icon.resolve(&numeric_tag("symbolrank", 9.0), 4).as_deref(), Some("dot-10"));
        assert_eq!(icon.resolve(&numeric_tag("symbolrank", 11.0), 4).as_deref(), Some("dot-9"));

        // From z8 the style asks for no icon at all -- the label stands on
        // its own text. `None`, never `Some("")`.
        assert_eq!(icon.resolve(&capital, 8), None);
        assert_eq!(icon.resolve(&numeric_tag("symbolrank", 5.0), 12), None);

        // `capital != 2` (a non-capital that still HAS the tag) must take
        // the else branch, same as a feature missing the tag entirely.
        let mut not_capital = numeric_tag("capital", 6.0);
        not_capital.insert("symbolrank".to_string(), TagValue::Number(11.0));
        assert_eq!(icon.resolve(&not_capital, 4).as_deref(), Some("dot-9"));
    }

    /// The Stage-B trap, pinned. `DataStepShape` selects a bucket by
    /// comparing a NUMERIC tag against `>=` thresholds, so a step keyed on
    /// STRING labels has no faithful encoding -- encoding it anyway would
    /// hand every feature the base bucket while appearing to work, and
    /// `settlement-subdivision-label` was deliberately left skipped over
    /// exactly that. The string-valued icon step here inherits the same
    /// limit and must refuse the same way: silent wrongness is worse than a
    /// skipped icon.
    #[test]
    fn refuses_a_data_driven_icon_step_whose_thresholds_are_string_labels() {
        let json = serde_json::json!(["step", ["zoom"],
            ["step", ["get", "type"], "dot-11", "suburb", "dot-9"],
            8, ""]);
        let error = compile_icon_image(&json).unwrap_err().to_string();
        assert!(
            error.contains("NUMERIC"),
            "the refusal must say why a string-keyed bucket can't be encoded, got: {error}"
        );
    }

    /// Same principle on the `case` half: the only test shape in scope is
    /// numeric equality against a property. Anything else (a `has`, a
    /// string comparison, an arithmetic test) is refused outright rather
    /// than approximated into something that quietly picks the wrong icon.
    #[test]
    fn refuses_a_data_driven_icon_case_whose_test_is_not_numeric_equality() {
        let json = serde_json::json!(["step", ["zoom"],
            ["case", ["has", "capital"], "border-dot-13", "dot-11"],
            8, ""]);
        let error = compile_icon_image(&json).unwrap_err().to_string();
        assert!(
            error.contains("numeric equality"),
            "the refusal must name the only test shape in scope, got: {error}"
        );
    }

    /// End-to-end over the real vendored style: the `icon-image` of BOTH
    /// city-label layers, pulled straight out of the style file rather than
    /// hand-copied, compiles and resolves to the real capital/symbolrank
    /// dots. (The layers themselves still skip on their `text-size`; see
    /// `the_real_settlement_label_layers_still_skip_on_their_text_size_not_their_icon_image`.)
    #[test]
    fn the_real_settlement_label_icon_images_compile_from_the_vendored_style() {
        for id in ["settlement-major-label", "settlement-minor-label"] {
            let layer = or_skip!(real_layer(id));
            let icon = compile_icon_image(&layer["layout"]["icon-image"])
                .unwrap_or_else(|e| panic!("{id}'s real icon-image must compile: {e:#}"));
            assert_eq!(icon.resolve(&numeric_tag("capital", 2.0), 5).as_deref(), Some("border-dot-13"));
            assert_eq!(icon.resolve(&numeric_tag("symbolrank", 10.0), 5).as_deref(), Some("dot-10"));
            assert_eq!(icon.resolve(&numeric_tag("symbolrank", 10.0), 9), None);

            // And through the whole-layer compiler, on the real layer with
            // only its (separately-blocked) text-size removed -- proving the
            // icon reaches `StyleLayer::Symbol`, not just the leaf compiler.
            let mut trimmed = layer.clone();
            trimmed["layout"].as_object_mut().unwrap().remove("text-size");
            let layers = compile_symbol_layer(&trimmed, Scope::root()).unwrap();
            assert_eq!(layers.len(), 1);
            let StyleLayer::Symbol { icon_image, .. } = &layers[0] else {
                panic!("expected a Symbol layer");
            };
            let icon_image = icon_image.as_ref().expect("the real icon-image must survive into the layer");
            assert_eq!(icon_image.resolve(&numeric_tag("symbolrank", 12.0), 3).as_deref(), Some("dot-9"));
        }
    }

    /// The whole reason `symbol-sort-key` is compiled at all: it is the
    /// within-layer collision priority, and the two city-label layers are
    /// exactly where labels pile up densely enough for that to matter.
    #[test]
    fn the_real_settlement_label_symbol_sort_keys_compile_from_the_vendored_style() {
        for id in ["settlement-major-label", "settlement-minor-label"] {
            let layers = compile_symbol_layer(&or_skip!(real_layer(id)), Scope::root())
                .unwrap_or_else(|e| panic!("{id} must compile: {e:#}"));
            assert_eq!(layers.len(), 1);
            let StyleLayer::Symbol { symbol_sort_key, .. } = &layers[0] else {
                panic!("expected a Symbol layer");
            };
            let key = symbol_sort_key
                .as_ref()
                .unwrap_or_else(|| panic!("{id}'s real [\"get\", \"symbolrank\"] sort key must compile"));
            // A lower symbolrank is a BIGGER city, and a lower sort key
            // places first -- the two conventions already agree, which is
            // why the property needs no inversion anywhere.
            assert_eq!(key.resolve(&numeric_tag("symbolrank", 6.0)), 6.0);
            assert_eq!(key.resolve(&numeric_tag("symbolrank", 15.0)), 15.0);
        }
    }

    #[test]
    fn a_literal_numeric_symbol_sort_key_compiles_to_the_same_value_for_every_feature() {
        let mut layer = or_skip!(real_layer("settlement-major-label"));
        layer["layout"]["symbol-sort-key"] = serde_json::json!(3);
        let layers = compile_symbol_layer(&layer, Scope::root()).expect("layer must compile");
        let StyleLayer::Symbol { symbol_sort_key, .. } = &layers[0] else {
            panic!("expected a Symbol layer");
        };
        let key = symbol_sort_key.as_ref().expect("a bare number must compile");
        assert_eq!(key.resolve(&numeric_tag("symbolrank", 6.0)), 3.0);
        assert_eq!(key.resolve(&HashMap::new()), 3.0);
    }

    /// A layer whose `symbol-sort-key` this compiler doesn't understand must
    /// keep every one of its labels. Failing the layer instead would
    /// soft-skip it entirely (see `compile_layer`'s `"symbol"` arm) -- and
    /// losing a whole layer of city names over an unreadable *tiebreak* is a
    /// far worse outcome than placing that layer's labels in source order.
    #[test]
    fn an_unsupported_symbol_sort_key_shape_is_treated_as_absent_and_never_costs_the_layer_its_labels() {
        let mut layer = or_skip!(real_layer("settlement-major-label"));
        layer["layout"]["symbol-sort-key"] =
            serde_json::json!(["*", ["get", "symbolrank"], 2]);
        let layers = compile_symbol_layer(&layer, Scope::root()).expect("an unreadable sort key must NOT fail the layer");
        assert_eq!(layers.len(), 1);
        let StyleLayer::Symbol { symbol_sort_key, text_field, .. } = &layers[0] else {
            panic!("expected a Symbol layer");
        };
        assert!(symbol_sort_key.is_none(), "an unsupported shape compiles to no sort key at all");
        assert!(text_field.is_some(), "and the layer keeps its text-field, which is the point");
    }

    #[test]
    fn a_layer_with_no_symbol_sort_key_at_all_compiles_to_none() {
        let mut layer = or_skip!(real_layer("settlement-major-label"));
        layer["layout"].as_object_mut().unwrap().remove("symbol-sort-key");
        let layers = compile_symbol_layer(&layer, Scope::root()).expect("layer must compile");
        let StyleLayer::Symbol { symbol_sort_key, .. } = &layers[0] else {
            panic!("expected a Symbol layer");
        };
        assert!(symbol_sort_key.is_none());
    }

    #[test]
    fn compiles_line_placement_with_its_companion_layout_properties() {
        let json = serde_json::json!({
            "id": "road-oneway-arrow",
            "type": "symbol",
            "source-layer": "road",
            "layout": {
                "symbol-placement": "line",
                "symbol-spacing": 200,
                "icon-image": "oneway-small",
                "icon-rotate": 180,
                "icon-rotation-alignment": "map",
                "icon-allow-overlap": true,
                "icon-ignore-placement": true
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("line placement must compile");
        let StyleLayer::Symbol { placement, spacing_px, icon_rotate_deg, rotation_alignment, bypass_collision, .. } =
            &layers[0]
        else {
            panic!("expected a symbol layer");
        };
        assert_eq!(placement.at(0), SymbolPlacement::Line);
        assert_eq!(spacing_px.resolve(&std::collections::HashMap::new(), 14), 200.0);
        assert_eq!(*icon_rotate_deg, 180.0);
        assert_eq!(*rotation_alignment, RotationAlignment::Map);
        assert!(*bypass_collision);
    }

    #[test]
    fn symbol_layout_defaults_match_the_style_spec() {
        let json = serde_json::json!({
            "id": "poi",
            "type": "symbol",
            "source-layer": "poi",
            "layout": {"text-field": ["get", "name"]}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("a point layer must compile");
        let StyleLayer::Symbol { placement, spacing_px, icon_rotate_deg, rotation_alignment, bypass_collision, .. } =
            &layers[0]
        else {
            panic!("expected a symbol layer");
        };
        assert_eq!(placement.at(0), SymbolPlacement::Point, "absent placement is point");
        assert_eq!(spacing_px.resolve(&std::collections::HashMap::new(), 14), 250.0, "style-spec default");
        assert_eq!(*icon_rotate_deg, 0.0);
        // Under POINT placement the spec's `auto` alignment resolves to viewport.
        assert_eq!(*rotation_alignment, RotationAlignment::Viewport);
        assert!(!*bypass_collision);
    }

    #[test]
    fn line_placement_defaults_rotation_alignment_to_map() {
        let json = serde_json::json!({
            "id": "arrow",
            "type": "symbol",
            "source-layer": "road",
            "layout": {"symbol-placement": "line", "icon-image": "arrow"}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("must compile");
        let StyleLayer::Symbol { rotation_alignment, .. } = &layers[0] else {
            panic!("expected a symbol layer");
        };
        // The style spec's `auto` resolves to `map` when placement is line --
        // which is what makes an arrow follow the road without the style saying so.
        assert_eq!(*rotation_alignment, RotationAlignment::Map);
    }

    /// The keyword names the part of the TEXT that lands on the anchor
    /// point, so `"left"` means "the text's left edge sits here, the rest
    /// runs to the right" -- which is how `poi_transit` keeps its label off
    /// its own icon.
    #[test]
    fn compiles_a_text_anchor_keyword() {
        for (keyword, expected) in [
            ("center", TextAnchor::Center),
            ("left", TextAnchor::Left),
            ("right", TextAnchor::Right),
            ("top", TextAnchor::Top),
            ("bottom", TextAnchor::Bottom),
            ("top-left", TextAnchor::TopLeft),
            ("top-right", TextAnchor::TopRight),
            ("bottom-left", TextAnchor::BottomLeft),
            ("bottom-right", TextAnchor::BottomRight),
        ] {
            let json = serde_json::json!({
                "id": "poi",
                "type": "symbol",
                "source-layer": "poi",
                "layout": {"text-field": ["get", "name"], "text-anchor": keyword}
            });
            let layers = compile_symbol_layer(&json, Scope::root()).expect("a text-anchor keyword must compile");
            let StyleLayer::Symbol { text_anchor, .. } = &layers[0] else {
                panic!("expected a symbol layer");
            };
            assert_eq!(text_anchor.resolve(&HashMap::new(), 15), expected, "{keyword}");
        }
    }

    /// Absent means the style-spec default, `center` -- today's behaviour
    /// for every label, which is the vast majority of them.
    #[test]
    fn an_absent_text_anchor_is_center() {
        let json = serde_json::json!({
            "id": "poi",
            "type": "symbol",
            "source-layer": "poi",
            "layout": {"text-field": ["get", "name"]}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("a point layer must compile");
        let StyleLayer::Symbol { text_anchor, .. } = &layers[0] else {
            panic!("expected a symbol layer");
        };
        assert_eq!(text_anchor.resolve(&HashMap::new(), 15), TextAnchor::Center);
    }

    /// The `line-dasharray` trade, applied to a layout property: a value
    /// this crate can't represent costs the layer its ANCHOR, not its
    /// symbols. Both shapes that reach here are covered -- an unknown
    /// keyword, and the expression form Mapbox Streets uses on five layers
    /// (`["match", ["get", "stop_type"], "entrance", "left", "top"]` on
    /// `transit-label`) which this compiler reads as a plain enum.
    #[test]
    fn an_unrepresentable_text_anchor_degrades_to_center_keeping_the_layer() {
        for bad in [
            serde_json::json!("middle-left"),
            serde_json::json!(["match", ["get", "stop_type"], "entrance", "left", "top"]),
            serde_json::json!(7),
        ] {
            let json = serde_json::json!({
                "id": "poi",
                "type": "symbol",
                "source-layer": "poi",
                "layout": {"text-field": ["get", "name"], "text-anchor": bad}
            });
            let layers = compile_symbol_layer(&json, Scope::root()).expect("an unreadable text-anchor must not sink the layer");
            assert_eq!(layers.len(), 1, "{bad}");
            let StyleLayer::Symbol { text_anchor, text_field, .. } = &layers[0] else {
                panic!("expected a symbol layer");
            };
            assert_eq!(text_anchor.resolve(&HashMap::new(), 15), TextAnchor::Center, "{bad}");
            assert!(text_field.is_some(), "the layer keeps everything else: {bad}");
        }
    }

    /// The real pairing this whole property exists for, from the vendored
    /// file: `poi_transit` anchors LEFT (its `text-offset` of `[0.9, 0]`
    /// then pushes the text clear of the transit sprite), while
    /// `poi_r1`/`poi_r7`/`poi_r20` anchor TOP so the name hangs below the
    /// icon. Before this compiled, both families centred their text on the
    /// anchor and drew it over their own icon -- confirmed on screen over
    /// Orchard Road, Singapore.
    #[test]
    fn the_liberty_poi_layers_compile_the_text_anchors_that_keep_labels_off_their_icons() {
        let style = liberty();
        let anchor_of = |id: &str| -> TextAnchor {
            style
                .layers
                .iter()
                .find_map(|layer| match layer {
                    StyleLayer::Symbol { id: layer_id, text_anchor, .. } if layer_id == id => {
                        Some(text_anchor.resolve(&HashMap::new(), 15))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Liberty has a symbol layer {id:?}"))
        };

        assert_eq!(anchor_of("poi_transit"), TextAnchor::Left);
        for id in ["poi_r1", "poi_r7", "poi_r20"] {
            assert_eq!(anchor_of(id), TextAnchor::Top, "{id}");
        }
    }

    /// THE acceptance case for "icons are on top of POI names like shops,
    /// banks": `poi-label`'s real `text-anchor` and `text-offset`, both from
    /// the vendored document, compiled together.
    ///
    /// Neither compiled before. The anchor -- a zoom `step` wrapping a
    /// `sizerank` `step` -- was read as "a keyword and nothing else" and
    /// degraded to `center`; the offset -- a zoom `step` wrapping a
    /// `sizerank` ramp -- had no per-feature ramp to compile into and
    /// degraded to nothing. Centred with no offset is the label drawn
    /// exactly on its own sprite, which is what the user saw.
    #[test]
    fn the_real_poi_label_anchors_its_name_below_its_icon() {
        let layer = or_skip!(standard_dark_layer("poi-label"));
        let compiled = compile_symbol_layer(&layer, Scope::root()).expect("poi-label must compile");
        let StyleLayer::Symbol { text_anchor, text_offset_x, text_offset_y, .. } = &compiled[0] else {
            panic!("expected a Symbol layer: {:?}", compiled[0]);
        };
        let ranked = |rank: f64| HashMap::from([("sizerank".to_string(), TagValue::Number(rank))]);

        // An ordinary POI at z17 (`sizerank` past that bracket's threshold
        // of 13): the label's TOP edge lands on the anchor point, and the
        // point itself has been pushed 0.75 em below the icon. Together
        // that is "the name hangs under the sprite" -- what Mapbox GL JS
        // draws at this camera.
        let ordinary = ranked(20.0);
        assert_eq!(text_anchor.resolve(&ordinary, 17), TextAnchor::Top);
        assert_eq!(text_offset_x.resolve(&ordinary, 17), 0.0);
        assert!((text_offset_y.resolve(&ordinary, 17) - 0.75).abs() < 1e-6);

        // A landmark important enough to clear the threshold keeps the
        // centred anchor and takes only part of the ramp -- the style's own
        // choice, not a fallback. 6/13 of 0.75.
        let landmark = ranked(6.0);
        assert_eq!(text_anchor.resolve(&landmark, 17), TextAnchor::Center);
        assert!(
            (text_offset_y.resolve(&landmark, 17) - 0.75 * 6.0 / 13.0).abs() < 1e-6,
            "got {}",
            text_offset_y.resolve(&landmark, 17)
        );

        // Below z17 the same layer uses a TIGHTER threshold (5, not 13), so
        // that same landmark hangs its label below the icon there. The zoom
        // bracket is carried, not resolved at compile time.
        assert_eq!(text_anchor.resolve(&landmark, 16), TextAnchor::Top);
        assert!((text_offset_y.resolve(&landmark, 16) - 0.75).abs() < 1e-6);
    }

    /// **The real Streets city label sits BESIDE its town dot, not on it.**
    /// The defect (the iPhone report: a dot on a letter in the middle of
    /// "Bengaluru", "Chennai", "Madurai"): `settlement-major-label`'s
    /// `text-anchor` is `["step", ["zoom"], ["get", "text_anchor"], 8,
    /// "center"]` -- the tileset's own keyword per town -- and a bracket
    /// that is a bare `["get", key]` was unreadable, so the whole property
    /// degraded to `center`; its `text-radial-offset` was not read at all.
    /// Centred with no offset is the name drawn straight over the dot.
    #[test]
    fn the_real_streets_city_label_is_anchored_beside_its_dot() {
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let root: Value = serde_json::from_slice(&bytes).expect("valid JSON");
        for id in ["settlement-major-label", "settlement-minor-label"] {
            let layer = root["layers"]
                .as_array()
                .and_then(|a| a.iter().find(|l| l["id"].as_str() == Some(id)))
                .expect("the vendored Streets style has the layer")
                .clone();
            let compiled = compile_symbol_layer(&layer, Scope::root()).expect("the layer compiles");
            let StyleLayer::Symbol { text_anchor, text_radial_offset, icon_image, .. } = &compiled[0] else {
                panic!("expected a Symbol layer: {:?}", compiled[0]);
            };
            let town = |anchor: &str, capital: f64| {
                HashMap::from([
                    ("text_anchor".to_string(), TagValue::String(anchor.to_string())),
                    ("capital".to_string(), TagValue::Number(capital)),
                    ("symbolrank".to_string(), TagValue::Number(8.0)),
                ])
            };
            let radial = text_radial_offset.as_ref().expect("text-radial-offset must be read, not dropped");
            assert!(icon_image.as_ref().and_then(|i| i.resolve(&town("left", 4.0), 5)).is_some_and(|n| !n.is_empty()));

            assert_eq!(text_anchor.resolve(&town("left", 4.0), 5), TextAnchor::Left, "{id}");
            assert_eq!(text_anchor.resolve(&town("bottom-right", 4.0), 5), TextAnchor::BottomRight, "{id}");
            assert_eq!(text_anchor.resolve(&town("nonsense", 4.0), 5), TextAnchor::Center, "{id}");
            assert_eq!(text_anchor.resolve(&HashMap::new(), 5), TextAnchor::Center, "{id}");
            assert_eq!(text_anchor.resolve(&town("left", 4.0), 8), TextAnchor::Center, "{id}: z8+ is centred");

            assert!((radial.resolve(&town("left", 4.0), 5) - 0.55).abs() < 1e-6, "{id}");
            assert!((radial.resolve(&town("left", 2.0), 5) - 0.6).abs() < 1e-6, "{id}: a capital");
            assert_eq!(radial.resolve(&town("left", 4.0), 8), 0.0, "{id}: no dot, no offset from z8");

            // Together: a `left`-anchored name's anchor point is moved
            // 0.55 em to the right of the dot, so its left edge starts
            // there and the run extends away from the dot.
            let offset = crate::style::point_text_offset(
                text_anchor.resolve(&town("left", 4.0), 5),
                radial.resolve(&town("left", 4.0), 5),
                (0.0, 0.0),
            );
            assert!((offset.0 - 0.55).abs() < 1e-6 && offset.1 == 0.0, "{id}: {offset:?}");
        }
    }

    /// The anchor half on its own, as the literal expression from the
    /// document -- a zoom `step` WRAPPING a data `step`, which is the shape
    /// that was missing.
    #[test]
    fn compiles_a_zoom_step_wrapping_a_data_step_text_anchor() {
        let json = serde_json::json!(["step", ["zoom"],
            ["step", ["get", "sizerank"], "center", 5, "top"],
            17,
            ["step", ["get", "sizerank"], "center", 13, "top"]
        ]);
        let value = compile_text_anchor(&json).expect("poi-label's real text-anchor must compile");
        let ranked = |rank: f64| HashMap::from([("sizerank".to_string(), TagValue::Number(rank))]);

        assert_eq!(value.resolve(&ranked(4.0), 16), TextAnchor::Center);
        assert_eq!(value.resolve(&ranked(5.0), 16), TextAnchor::Top);
        // The z17 bracket moves the threshold, so the same feature flips back.
        assert_eq!(value.resolve(&ranked(5.0), 17), TextAnchor::Center);
        assert_eq!(value.resolve(&ranked(13.0), 17), TextAnchor::Top);
        // No `sizerank` at all takes the step's base, as real `step`
        // semantics do for a null input.
        assert_eq!(value.resolve(&HashMap::new(), 17), TextAnchor::Center);
    }

    /// A mixed declaration -- one zoom bracket a bare keyword, the next a
    /// data step -- needs no second variant: a keyword compiles to a step
    /// with no stops.
    #[test]
    fn compiles_a_zoom_stepped_text_anchor_whose_brackets_mix_keywords_and_data_steps() {
        let json = serde_json::json!(["step", ["zoom"],
            "left",
            17,
            ["step", ["get", "sizerank"], "center", 13, "top"]
        ]);
        let value = compile_text_anchor(&json).expect("a mixed-bracket text-anchor must compile");
        let ranked = |rank: f64| HashMap::from([("sizerank".to_string(), TagValue::Number(rank))]);
        assert_eq!(value.resolve(&ranked(20.0), 10), TextAnchor::Left);
        assert_eq!(value.resolve(&ranked(20.0), 17), TextAnchor::Top);
    }

    /// The number is a multiple of the FONT SIZE, not a pixel count -- the
    /// style spec's unit for this property is ems -- so the same `9` wraps
    /// at 90px on a 10px label and 180px on a 20px one.
    #[test]
    fn compiles_a_text_max_width_in_ems() {
        let json = serde_json::json!({
            "id": "poi",
            "type": "symbol",
            "source-layer": "poi",
            "layout": {"text-field": ["get", "name"], "text-max-width": 9}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("a numeric text-max-width must compile");
        let StyleLayer::Symbol { text_max_width, .. } = &layers[0] else {
            panic!("expected a symbol layer");
        };
        assert_eq!(*text_max_width, 9.0);
    }

    /// Absent means the style-spec default of 10 em. It is NOT "unbounded":
    /// a style that says nothing still asks for wrapping, which is why
    /// `Bef Yio Chu Kang Stn Exit A` had no business rendering as one
    /// unbroken line before this compiled.
    #[test]
    fn an_absent_text_max_width_is_the_spec_default_of_ten_ems() {
        let json = serde_json::json!({
            "id": "poi",
            "type": "symbol",
            "source-layer": "poi",
            "layout": {"text-field": ["get", "name"]}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("a point layer must compile");
        let StyleLayer::Symbol { text_max_width, .. } = &layers[0] else {
            panic!("expected a symbol layer");
        };
        assert_eq!(*text_max_width, DEFAULT_TEXT_MAX_WIDTH_EM);
        assert_eq!(*text_max_width, 10.0, "and concretely, so a changed constant cannot pass this silently");
    }

    /// The `line-dasharray` trade again, and the same one `text-anchor`
    /// makes: a width this crate can't read costs the layer its WRAP
    /// WIDTH, not its symbols. Both non-numeric shapes that reach here are
    /// covered -- the expression form Mapbox Streets uses on two layers
    /// (`water-point-label`'s `["match", ["get", "class"], "ocean", 4,
    /// ...]` and `transit-label`'s stop-type match) and a plain junk value.
    #[test]
    fn an_unrepresentable_text_max_width_degrades_to_the_default_keeping_the_layer() {
        for bad in [
            serde_json::json!(["match", ["get", "class"], "ocean", 4, "sea", 5, 10]),
            serde_json::json!("nine"),
            // Zero and negatives are numbers the spec's own `minimum: 0`
            // rules out, and a zero wrap width would break every label to
            // one word per line.
            serde_json::json!(0),
            serde_json::json!(-3),
        ] {
            let json = serde_json::json!({
                "id": "poi",
                "type": "symbol",
                "source-layer": "poi",
                "layout": {"text-field": ["get", "name"], "text-max-width": bad}
            });
            let layers = compile_symbol_layer(&json, Scope::root()).expect("an unreadable text-max-width must not sink the layer");
            assert_eq!(layers.len(), 1, "{bad}");
            let StyleLayer::Symbol { text_max_width, text_field, .. } = &layers[0] else {
                panic!("expected a symbol layer");
            };
            assert_eq!(*text_max_width, DEFAULT_TEXT_MAX_WIDTH_EM, "{bad}");
            assert!(text_field.is_some(), "the layer keeps everything else: {bad}");
        }
    }

    /// The real declarations this property exists for, from the vendored
    /// file. Every Liberty POI layer asks for 9 em, and before this
    /// compiled they all rendered as one unbroken line however long the
    /// name -- `Opposite Orchard Station Exit 12` ran clean across its
    /// neighbours over Singapore.
    #[test]
    fn the_liberty_poi_layers_compile_the_nine_em_text_max_width_that_wraps_their_names() {
        let style = liberty();
        let max_width_of = |id: &str| -> f32 {
            style
                .layers
                .iter()
                .find_map(|layer| match layer {
                    StyleLayer::Symbol { id: layer_id, text_max_width, .. } if layer_id == id => Some(*text_max_width),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("Liberty has a symbol layer {id:?}"))
        };

        for id in ["poi_transit", "poi_r1", "poi_r7", "poi_r20"] {
            assert_eq!(max_width_of(id), 9.0, "{id}");
        }
        // A fractional declaration reaches the field intact -- Liberty's
        // country labels really do ask for 6.25 em.
        assert_eq!(max_width_of("label_country_1"), 6.25);
    }

    #[test]
    fn an_unrepresentable_layout_value_degrades_without_dropping_the_layer() {
        let json = serde_json::json!({
            "id": "arrow",
            "type": "symbol",
            "source-layer": "road",
            "layout": {
                "symbol-placement": "line",
                "icon-image": "arrow",
                // A bare data-driven ["get", ...] with no "step"/"interpolate"
                // wrapper: still not a shape compile_layout_value understands
                // (unlike a zoom interpolate, which now compiles -- see
                // compiles_a_zoom_interpolated_symbol_spacing below).
                "symbol-spacing": ["get", "spacing"],
                "icon-rotate": ["get", "bearing"]
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("layer must survive a value it cannot represent");
        let StyleLayer::Symbol { spacing_px, icon_rotate_deg, .. } = &layers[0] else {
            panic!("expected a symbol layer");
        };
        let tags = std::collections::HashMap::new();
        assert_eq!(spacing_px.resolve(&tags, 16), 250.0, "degrades to the default, not an error");
        assert_eq!(*icon_rotate_deg, 0.0);
    }

    #[test]
    fn compiles_a_zoom_interpolated_symbol_spacing() {
        let json = serde_json::json!({
            "id": "shield", "type": "symbol", "source-layer": "road",
            "layout": {
                "symbol-spacing": ["interpolate", ["linear"], ["zoom"], 11, 400, 14, 600],
                "icon-image": "x"
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("interpolated spacing must compile");
        let StyleLayer::Symbol { spacing_px, .. } = &layers[0] else { panic!("expected a symbol layer") };
        let tags = std::collections::HashMap::new();
        assert_eq!(spacing_px.resolve(&tags, 11), 400.0);
        assert_eq!(spacing_px.resolve(&tags, 14), 600.0);
        // Halfway along the curve in zoom terms: 11 -> 14 is 3 levels, so z12.5
        // is not an integer zoom; the plan resolves at integer zooms only, so
        // check z12 sits strictly between the endpoints instead.
        let mid = spacing_px.resolve(&tags, 12);
        assert!(mid > 400.0 && mid < 600.0, "z12 spacing was {mid}");
    }

    #[test]
    fn a_constant_symbol_spacing_still_compiles_and_absence_still_defaults() {
        let json = serde_json::json!({
            "id": "a", "type": "symbol", "source-layer": "road",
            "layout": {"symbol-spacing": 200, "icon-image": "x"}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("must compile");
        let StyleLayer::Symbol { spacing_px, .. } = &layers[0] else { panic!("expected a symbol layer") };
        assert_eq!(spacing_px.resolve(&std::collections::HashMap::new(), 16), 200.0);

        let json = serde_json::json!({
            "id": "b", "type": "symbol", "source-layer": "road",
            "layout": {"icon-image": "x"}
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("must compile");
        let StyleLayer::Symbol { spacing_px, .. } = &layers[0] else { panic!("expected a symbol layer") };
        assert_eq!(spacing_px.resolve(&std::collections::HashMap::new(), 16), 250.0);
    }

    #[test]
    fn compiles_a_zoom_stepped_symbol_placement() {
        let json = serde_json::json!({
            "id": "shield", "type": "symbol", "source-layer": "road",
            "layout": {
                "symbol-placement": ["step", ["zoom"], "point", 11, "line"],
                "icon-image": "x"
            }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("stepped placement must compile now");
        let StyleLayer::Symbol { placement, .. } = &layers[0] else { panic!("expected a symbol layer") };
        assert_eq!(placement.at(10), SymbolPlacement::Point);
        assert_eq!(placement.at(11), SymbolPlacement::Line);
        assert_eq!(placement.at(12), SymbolPlacement::Line);
    }

    #[test]
    fn a_constant_symbol_placement_still_compiles() {
        for (declared, expected) in [("point", SymbolPlacement::Point), ("line", SymbolPlacement::Line)] {
            let json = serde_json::json!({
                "id": "l", "type": "symbol", "source-layer": "road",
                "layout": {"symbol-placement": declared, "icon-image": "x"}
            });
            let layers = compile_symbol_layer(&json, Scope::root()).expect("constant placement must compile");
            let StyleLayer::Symbol { placement, .. } = &layers[0] else { panic!("expected a symbol layer") };
            assert_eq!(placement.at(0), expected);
            assert_eq!(placement.at(22), expected);
        }
    }

    #[test]
    fn a_stepped_placement_with_an_unknown_keyword_is_rejected() {
        let json = serde_json::json!({
            "id": "l", "type": "symbol", "source-layer": "road",
            "layout": {"symbol-placement": ["step", ["zoom"], "point", 11, "line-middle"], "icon-image": "x"}
        });
        let error = compile_symbol_layer(&json, Scope::root()).expect_err("line-center is out of scope");
        assert!(format!("{error:#}").contains("symbol-placement"));
    }

    fn line_dash_at(layer: &StyleLayer, zoom: u8) -> DashPattern {
        match layer {
            StyleLayer::Line { dash, .. } => dash.value_at(zoom),
            other => panic!("expected a line layer, got {other:?}"),
        }
    }

    #[test]
    fn compiles_a_literal_line_dasharray() {
        let json = serde_json::json!({
            "id": "t", "type": "line", "source-layer": "road",
            "paint": { "line-color": "#000", "line-dasharray": [3, 3] }
        });
        let layers = compile_layer("t", "line", &json, Scope::root()).expect("layer compiles");
        assert_eq!(line_dash_at(&layers[0], 10), DashPattern::from_runs(&[3.0, 3.0]));
    }

    #[test]
    fn compiles_a_zoom_stepped_line_dasharray() {
        // admin-1-boundary's real pattern: solid below z7, dash-dot at and above.
        let json = serde_json::json!({
            "id": "t", "type": "line", "source-layer": "admin",
            "paint": {
                "line-color": "#000",
                "line-dasharray": ["step", ["zoom"],
                    ["literal", [2, 0]], 7, ["literal", [2, 2, 6, 2]]]
            }
        });
        let layers = compile_layer("t", "line", &json, Scope::root()).expect("layer compiles");
        assert_eq!(line_dash_at(&layers[0], 6), DashPattern::from_runs(&[2.0, 0.0]));
        assert_eq!(line_dash_at(&layers[0], 7), DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0]));
        assert_eq!(line_dash_at(&layers[0], 12), DashPattern::from_runs(&[2.0, 2.0, 6.0, 2.0]));
    }

    #[test]
    fn a_line_layer_without_dasharray_is_not_dashed() {
        let json = serde_json::json!({
            "id": "t", "type": "line", "source-layer": "road",
            "paint": { "line-color": "#000" }
        });
        let layers = compile_layer("t", "line", &json, Scope::root()).expect("layer compiles");
        assert_eq!(line_dash_at(&layers[0], 10).count, 0);
    }

    // The sharpest edge in this feature: `try_or_skip!` drops the WHOLE LAYER on
    // Err. An unparseable dasharray must never do that.
    #[test]
    fn a_malformed_line_dasharray_keeps_the_layer_and_renders_solid() {
        for bad in [
            serde_json::json!("not-an-array"),
            serde_json::json!([1, 2, 3, 4, 5, 6]),
            serde_json::json!(["zoom"]),
            serde_json::json!([]),
            // The branch `ZoomExprValue::INTERPOLATOR == None` exists for:
            // `DashPattern` cannot be blended, so `compile_interpolate` bails
            // rather than misrendering. That `Err` must be absorbed here, not
            // allowed to reach `try_or_skip!`.
            serde_json::json!(["interpolate", ["linear"], ["zoom"], 5, ["literal", [2, 2]], 10, ["literal", [4, 4]]]),
        ] {
            let json = serde_json::json!({
                "id": "t", "type": "line", "source-layer": "road",
                "paint": { "line-color": "#000", "line-dasharray": bad }
            });
            let layers = compile_layer("t", "line", &json, Scope::root())
                .unwrap_or_else(|e| panic!("layer must still compile for {bad}: {e}"));
            assert_eq!(layers.len(), 1, "layer must not be dropped for {bad}");
            assert_eq!(line_dash_at(&layers[0], 10).count, 0, "must render solid for {bad}");
        }
    }


    // The second sharpest edge, and the one the Mapbox Standard work turns
    // on: an expression the constant folder recognizes but cannot reduce
    // must degrade THE PROPERTY, never drop the layer. The imported Standard
    // document reaches for the folder in 796 places, so "skip the layer"
    // would delete most of the style.

    fn fill_color_at(layer: &StyleLayer, zoom: u8) -> Rgba {
        match layer {
            StyleLayer::Fill { color, .. } => color.value_at(zoom),
            other => panic!("expected a fill layer, got {other:?}"),
        }
    }

    #[test]
    fn a_feature_dependent_let_binding_splits_into_filtered_layers() {
        // `roads`' real `isHighClass` shape (see `compile_let`'s doc
        // comment): a `let` binds a boolean `match` on a feature property,
        // then a `case` branches on it. This USED to degrade the whole
        // property to a literal (`let_bindings` categorically refused any
        // feature-dependent binding); `compile_let` now inlines it
        // instead, so `compile_layer`'s ordinary `fill-color` cross-
        // product expansion produces one filtered `StyleLayer::Fill` per
        // case, exactly as it already does for a bare
        // `["match", ["get", "class"], ...]`.
        let json = serde_json::json!({
            "id": "roads", "type": "fill", "source-layer": "road",
            "paint": { "fill-color": ["let",
                "isHighClass", ["match", ["get", "class"], ["motorway", "trunk"], true, false],
                ["case", ["var", "isHighClass"], "#ff0000", "#00ff00"]] }
        });
        let _ = take_degradations();
        let layers = compile_layer("roads", "fill", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 2, "one layer per case-branch, filtered by class: {layers:?}");
        assert!(take_degradations().is_empty(), "this now folds cleanly -- no degradation");
        let motorway_or_trunk_color = layers.iter().find_map(|layer| match layer {
            StyleLayer::Fill { filter: Filter::In { key, values }, color, .. }
                if key == "class" && values == &["motorway".to_string(), "trunk".to_string()] =>
            {
                Some(color.value_at(10))
            }
            _ => None,
        });
        assert_eq!(motorway_or_trunk_color, Some(Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }), "layers: {layers:?}");
        let fallback_color = layers.iter().find_map(|layer| match layer {
            StyleLayer::Fill { filter: Filter::Not(inner), color, .. } => match inner.as_ref() {
                Filter::In { key, values } if key == "class" && values == &["motorway".to_string(), "trunk".to_string()] => {
                    Some(color.value_at(10))
                }
                _ => None,
            },
            _ => None,
        });
        assert_eq!(fallback_color, Some(Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }), "layers: {layers:?}");
    }

    #[test]
    fn an_unfoldable_fill_color_degrades_while_the_rest_of_the_layer_still_compiles() {
        // `config` is not resolved until the next commit; every fill in the
        // Standard document is written this way.
        let json = serde_json::json!({
            "id": "land", "type": "fill", "source-layer": "landuse",
            "paint": {
                "fill-color": ["at", 0, ["to-hsla", ["config", "colorLand"]]],
                "fill-opacity": 0.5
            }
        });
        let layers = compile_layer("land", "fill", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 1);
        match &layers[0] {
            StyleLayer::Fill { opacity, .. } => assert_eq!(opacity.value_at(10), 0.5, "the other property is untouched"),
            other => panic!("expected a fill layer, got {other:?}"),
        }
    }

    #[test]
    fn an_unfoldable_line_width_degrades_to_the_style_spec_default() {
        let json = serde_json::json!({
            "id": "road", "type": "line", "source-layer": "road",
            "paint": { "line-color": "#123456", "line-width": ["*", ["config", "roadScale"], 2] }
        });
        let layers = compile_layer("road", "line", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 1);
        match &layers[0] {
            StyleLayer::Line { width, color, .. } => {
                assert_eq!(width.value_at(10), 1.0, "the style-spec default line-width");
                assert_eq!(color.value_at(10), parse_css_color("#123456").unwrap(), "the colour still compiles");
            }
            other => panic!("expected a line layer, got {other:?}"),
        }
    }

    #[test]
    fn an_unfoldable_background_color_degrades_and_keeps_the_layer() {
        let json = serde_json::json!({
            "id": "land", "type": "background",
            "paint": { "background-color": ["let",
                "h", ["at", 0, ["to-hsla", ["config", "colorLand"]]],
                ["hsl", ["var", "h"], 20, 95]] }
        });
        let layers = compile_layer("land", "background", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 1);
    }

    // ---- *-emissive-strength (Task 1 of the lighting-model series) ----

    /// The compiled `*-emissive-strength` cases for one layer's paint
    /// property, read where that value lives now: the compile step itself.
    /// It is folded into the layer's colour (`light_ground_color`) and
    /// deliberately NOT stored on the compiled `StyleLayer` -- a stored copy
    /// of a value that has already been applied is a standing invitation to
    /// apply it twice.
    ///
    /// `default` is the property's own style-spec default, which is not
    /// uniform: 0 for background/fill/line, 1 for icon/text.
    fn compiled_emissive(layer: &Value, property: &str, default: f32, scope: Scope<'_>) -> Vec<(Filter, ZoomValue<f32>)> {
        let id = layer["id"].as_str().unwrap_or("<test layer>");
        compile_emissive_strength(layer.get("paint").and_then(|paint| paint.get(property)), id, property, default, scope)
    }

    /// `compiled_emissive` for a property that does not vary per feature --
    /// the usual case -- resolved at `zoom`.
    fn compiled_emissive_value(layer: &Value, property: &str, default: f32, scope: Scope<'_>, zoom: u8) -> f32 {
        let cases = compiled_emissive(layer, property, default, scope);
        assert_eq!(cases.len(), 1, "{property} was expected not to vary per feature, got {cases:?}");
        cases[0].1.value_at(zoom)
    }

    #[test]
    fn background_emissive_strength_defaults_to_zero_when_absent() {
        let json = serde_json::json!({
            "id": "land", "type": "background",
            "paint": { "background-color": "#101010" }
        });
        let layers = compile_layer("land", "background", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 1);
        let strength = compiled_emissive_value(&json, "background-emissive-strength", 0.0, Scope::root(), 10);
        assert_eq!(strength, 0.0, "background-emissive-strength's style-spec default");
    }

    #[test]
    fn fill_emissive_strength_defaults_to_zero_and_compiles_a_declared_value() {
        let json = serde_json::json!({
            "id": "land", "type": "fill", "source-layer": "landuse",
            "paint": { "fill-color": "#123456" }
        });
        compile_layer("land", "fill", &json, Scope::root()).expect("the layer must survive");
        let strength = compiled_emissive_value(&json, "fill-emissive-strength", 0.0, Scope::root(), 10);
        assert_eq!(strength, 0.0, "fill-emissive-strength's style-spec default");

        let json = serde_json::json!({
            "id": "land", "type": "fill", "source-layer": "landuse",
            "paint": { "fill-color": "#123456", "fill-emissive-strength": 0.42 }
        });
        compile_layer("land", "fill", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(compiled_emissive_value(&json, "fill-emissive-strength", 0.0, Scope::root(), 10), 0.42);
    }

    #[test]
    fn line_emissive_strength_defaults_to_zero_and_compiles_a_declared_value() {
        let json = serde_json::json!({
            "id": "road", "type": "line", "source-layer": "road",
            "paint": { "line-color": "#123456" }
        });
        compile_layer("road", "line", &json, Scope::root()).expect("the layer must survive");
        let strength = compiled_emissive_value(&json, "line-emissive-strength", 0.0, Scope::root(), 10);
        assert_eq!(strength, 0.0, "line-emissive-strength's style-spec default");

        let json = serde_json::json!({
            "id": "road", "type": "line", "source-layer": "road",
            "paint": { "line-color": "#123456", "line-emissive-strength": 0.7 }
        });
        compile_layer("road", "line", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(compiled_emissive_value(&json, "line-emissive-strength", 0.0, Scope::root(), 10), 0.7);
    }

    #[test]
    fn a_feature_dependent_line_emissive_strength_splits_into_filtered_layers() {
        // `roads`' real `line-emissive-strength` shape: a `let` that binds
        // `isHighClass` to a feature-dependent `match` on `["get", "class"]`,
        // then branches a `case` on it. `compile_let` now inlines a
        // feature-dependent binding instead of refusing it (see its doc
        // comment), turning the `case` into an ordinary feature-dependent
        // expression -- here, directly as the property (no enclosing
        // arithmetic), so this splits via `compile_zoom_value`'s ordinary
        // `case` dispatch with no help from `hoist_feature_dependent_case`
        // needed. The exact `roads`/`bridge-roads` real expression --
        // arithmetic-wrapped, needing that hoist -- is
        // `the_real_roads_line_emissive_strength_resolves_by_class`, below.
        let json = serde_json::json!({
            "id": "roads", "type": "line", "source-layer": "road",
            "paint": {
                "line-color": "#123456",
                "line-emissive-strength": ["let",
                    "isHighClass", ["match", ["get", "class"], ["motorway", "trunk"], true, false],
                    ["case", ["var", "isHighClass"], 0.3, 0.5]]
            }
        });
        let _ = take_degradations();
        let layers = compile_layer("roads", "line", &json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 2, "one layer per case-branch, filtered by class: {layers:?}");
        assert!(take_degradations().is_empty(), "this now folds cleanly -- no degradation");
        for layer in &layers {
            let StyleLayer::Line { color, .. } = layer else { panic!("expected a line layer") };
            assert_eq!(color.value_at(10), parse_css_color("#123456").unwrap(), "the rest of the layer is untouched");
        }
        // The strength each of those two layers was compiled with, read at
        // the compile step (it is folded into the colour, not stored).
        for (filter, strength) in compiled_emissive(&json, "line-emissive-strength", 0.0, Scope::root()) {
            let expected = match &filter {
                Filter::In { key, values } if key == "class" && values == &["motorway".to_string(), "trunk".to_string()] => 0.3,
                Filter::Not(inner) => match inner.as_ref() {
                    Filter::In { key, values } if key == "class" && values == &["motorway".to_string(), "trunk".to_string()] => 0.5,
                    other => panic!("unexpected fallback filter: {other:?}"),
                },
                other => panic!("unexpected filter: {other:?}"),
            };
            assert_eq!(strength.value_at(10), expected, "filter: {filter:?}");
        }
    }

    #[test]
    fn the_real_roads_line_emissive_strength_resolves_by_class() {
        // The full real shape, arithmetic-wrapped: `roads`'/`bridge-roads`'
        // actual `line-emissive-strength` is `roadsBrightness -
        // case(isHighClass, 0.1, 0.2)` under `theme: "monochrome"`
        // (`roadsBrightness` defaults to 0.4, unless overridden). Unlike
        // the test above, `isHighClass`'s `case` here is nested one level
        // inside the `-`, so this needs `hoist_feature_dependent_case` to
        // reach `compile_zoom_value`'s `case` dispatch at all.
        let json = serde_json::json!({
            "id": "roads", "type": "line", "source-layer": "road",
            "paint": {
                "line-color": "#123456",
                "line-emissive-strength": ["let",
                    "brightness", ["config", "roadsBrightness"],
                    "isHighClass", ["match", ["get", "class"], ["motorway", "motorway_link", "trunk", "trunk_link"], true, false],
                    ["match", ["config", "theme"], "monochrome",
                        ["-", ["var", "brightness"], ["case", ["var", "isHighClass"], 0.1, 0.2]],
                        ["+", ["var", "brightness"], ["case", ["var", "isHighClass"], 0.2, 0]]]]
            }
        });
        let config = ResolvedConfig::from_pairs([
            ("theme", serde_json::json!("monochrome")),
            ("roadsBrightness", serde_json::json!(0.4)),
        ]);
        let scope = Scope::root().with_config(&config);
        let _ = take_degradations();
        let layers = compile_layer("roads", "line", &json, scope).expect("the layer must survive");
        assert_eq!(layers.len(), 2, "one layer per class-based case: {layers:?}");
        assert!(take_degradations().is_empty(), "this now folds cleanly -- no degradation");
        let cases = compiled_emissive(&json, "line-emissive-strength", 0.0, scope);
        let motorway = cases.iter().find_map(|(filter, strength)| match filter {
            Filter::In { key, values }
                if key == "class" && values == &["motorway".to_string(), "motorway_link".to_string(), "trunk".to_string(), "trunk_link".to_string()] =>
            {
                Some(strength.value_at(10))
            }
            _ => None,
        });
        assert_eq!(motorway, Some(0.3), "0.4 (roadsBrightness) - 0.1: cases: {cases:?}");
        let residential = cases.iter().find_map(|(filter, strength)| match filter {
            Filter::Not(_) => Some(strength.value_at(10)),
            _ => None,
        });
        assert_eq!(residential, Some(0.2), "0.4 (roadsBrightness) - 0.2: cases: {cases:?}");
    }

    #[test]
    fn icon_and_text_emissive_strength_default_to_one_and_compile_declared_values() {
        let json = serde_json::json!({
            "id": "poi", "type": "symbol", "source-layer": "poi",
            "layout": { "text-field": "{name}" },
            "paint": {}
        });
        compile_symbol_layer(&json, Scope::root()).expect("the layer must survive");
        let icon = compiled_emissive_value(&json, "icon-emissive-strength", 1.0, Scope::root(), 10);
        let text = compiled_emissive_value(&json, "text-emissive-strength", 1.0, Scope::root(), 10);
        assert_eq!(icon, 1.0, "icon-emissive-strength's style-spec default");
        assert_eq!(text, 1.0, "text-emissive-strength's style-spec default");

        let json = serde_json::json!({
            "id": "poi", "type": "symbol", "source-layer": "poi",
            "layout": { "text-field": "{name}" },
            "paint": { "icon-emissive-strength": 0.2, "text-emissive-strength": 0.9 }
        });
        compile_symbol_layer(&json, Scope::root()).expect("the layer must survive");
        assert_eq!(compiled_emissive_value(&json, "icon-emissive-strength", 1.0, Scope::root(), 10), 0.2);
        assert_eq!(compiled_emissive_value(&json, "text-emissive-strength", 1.0, Scope::root(), 10), 0.9);
    }

    /// The task-1 acceptance case, verbatim: `land`'s real
    /// `background-emissive-strength` -- `["interpolate", ["linear"],
    /// ["zoom"], 13, 0.1, 14, ["interpolate", ["linear"], ["measure-light",
    /// "brightness"], ...]]` -- resolves to 0.1 at zoom 13 (the flat literal
    /// stop) and 0.05 at zoom 14 under the fixture's own `night` +
    /// `monochrome` config, where both lights are black (brightness 0, the
    /// monochrome branch's zero-brightness case).
    ///
    /// Read at the compile step against the real fixture's own `land` layer,
    /// in the real import's scope (its resolved `config` and the document's
    /// `lights`, which the `measure-light` stop needs) -- the strength is
    /// folded into the compiled colour rather than stored on the layer. Its
    /// effect on that colour is pinned separately and end to end by
    /// `the_vendored_dark_fixtures_land_background_renders_dark_at_night`,
    /// where these two numbers are exactly the 24 and 12 it asserts.
    #[test]
    fn the_vendored_dark_fixtures_land_emissive_strength_resolves_across_zoom() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = parse_imports(&root).unwrap();
        let import = &imports[0];
        let scope = Scope::root().with_config(&import.config).with_lights(&import.data["lights"]);
        let land = import.data["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|layer| layer["id"] == serde_json::json!("land"))
            .expect("the imported basemap declares a \"land\" background");

        let at = |zoom| compiled_emissive_value(land, "background-emissive-strength", 0.0, scope, zoom);
        assert_eq!(at(13), 0.1);
        assert!((at(14) - 0.05).abs() < 1e-6, "expected ~0.05 at zoom 14 under night + monochrome, got {}", at(14));
    }

    // ---- Ground lighting (task 3 of the lighting-model series) ----------
    //
    // `_prelude_lighting.glsl`'s `apply_lighting_with_emission_ground`,
    // Mapbox GL JS 3.15.0:
    //
    //     lit_c = color_c * mix(ground_radiance_c, 1.0, emissive_strength)
    //     lit_a = color_a
    //
    // baked into the compiled colour at every integer zoom, since colour,
    // emissive strength and ground radiance are all functions of zoom alone.

    /// The vendored Standard fixture, recompiled with one `lightPreset`
    /// swapped in. Everything else -- `theme: monochrome`, `colorLand`, the
    /// whole `lights` array -- stays exactly as shipped.
    fn standard_fixture_under_preset(preset: &str) -> Option<crate::style::Style> {
        let bytes = crate::mapbox_fixtures::read("mapbox-standard-dark-style.json")?;
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        root["imports"][0]["config"]["lightPreset"] = serde_json::json!(preset);
        Some(parse_and_compile(root.to_string().as_bytes()).expect("the fixture must compile"))
    }

    fn land_background_color(style: &crate::style::Style) -> &ZoomValue<Rgba> {
        style
            .layers
            .iter()
            .find_map(|l| match l {
                StyleLayer::Background { id, color, .. } if id == "land" => Some(color),
                _ => None,
            })
            .expect("the imported basemap's background must compile")
    }

    /// Every channel within a rounding error of `want`. Colour arithmetic
    /// here runs through `f32` multiplies, so exact equality would pin the
    /// order of operations rather than the value.
    fn assert_rgba_close(got: Rgba, want: Rgba, what: &str) {
        assert_rgba_close_within(got, want, 1e-6, what);
    }

    /// The same, with the tolerance spelled out -- for the one caller that
    /// compares two whole styles' worth of colours through an `f32` lerp
    /// (`an_identity_color_theme_leaves_every_colour_where_it_was`).
    fn assert_rgba_close_within(got: Rgba, want: Rgba, tolerance: f32, what: &str) {
        let channels = [("r", got.r, want.r), ("g", got.g, want.g), ("b", got.b, want.b), ("a", got.a, want.a)];
        for (channel, got, want) in channels {
            assert!((got - want).abs() < tolerance, "{what} channel {channel}: got {got}, want {want}");
        }
    }

    /// `color`'s RGB scaled by `factor`, alpha untouched -- the shape every
    /// expectation in these lighting tests takes.
    fn scale_rgb(color: Rgba, factor: f32) -> Rgba {
        Rgba { r: color.r * factor, g: color.g * factor, b: color.b * factor, a: color.a }
    }

    /// The colours these lighting expectations START from, once the
    /// vendored fixture's own `theme: monochrome` `color-theme` cube has
    /// graded them.
    ///
    /// The grade runs BEFORE the lighting multiply (see
    /// `themed_and_lit_color`), so every expectation below is a lighting
    /// factor times one of these, not times the authored colour. That
    /// ordering is what these constants encode, and it is why they are here
    /// rather than in the LUT's own tests: swap the two operations and every
    /// number in this section moves.
    ///
    /// Each is a literal, computed INDEPENDENTLY of this crate -- a
    /// standalone trilinear sample of the fixture's decoded 32x1024 LUT PNG,
    /// to nine places. Deliberately not `ColorLut::apply`, which would have
    /// these tests predict the code under test with the code under test.
    ///
    /// They are all pure greys, and that is not a simplification: Standard's
    /// `monochrome` cube maps every input to the neutral axis, which is the
    /// whole reason it exists. It is a grade, not merely a desaturation --
    /// `hsl(0, 0%, 76%)` comes back BRIGHTER, at 0.850, not unchanged.
    mod graded {
        use crate::style::Rgba;

        const fn grey(value: f32) -> Rgba {
            Rgba { r: value, g: value, b: value, a: 1.0 }
        }

        /// `colorLand`'s schema default `hsl(20, 20%, 95%)`.
        pub const LAND: Rgba = grey(0.985_999_5);

        /// `colorWater`'s override `hsl(0, 0%, 76%)`.
        pub const WATER: Rgba = grey(0.850_125_6);

        /// `water`'s zoom-14 ramp value under the light
        /// `waters_darkening_branch_survives_compilation_under_a_light_that_reaches_it`
        /// contrives: `lerp(hsl(0, 0%, 21%), hsl(0, 0%, 76%), 0.2483)`,
        /// i.e. 0.346565, graded. The blend happens while the colour
        /// expression folds, so what the cube sees is the ALREADY-blended
        /// stop, not its two endpoints.
        pub const WATER_DARKENED_BLEND: Rgba = grey(0.514_527_9);
    }

    /// EXACTLY what the renderer writes for an 8-bit colour: `(c * 255.0) as
    /// u8` (`map_view.rs`'s glyph colour), which TRUNCATES -- and saturates,
    /// which is the one and only clamp in the whole lighting path (see
    /// `lighting_leaves_a_channel_above_one_unclamped`).
    fn u8_written(color: Rgba) -> [u8; 3] {
        [(color.r * 255.0) as u8, (color.g * 255.0) as u8, (color.b * 255.0) as u8]
    }

    /// The same colour to the NEAREST 8-bit value, which is the unit the
    /// plan states its acceptance criteria in ("rgb(12,12,12)"). It differs
    /// from `u8_written` by at most one count, and does so exactly when a
    /// channel lands just under a boundary: `land`'s blue at night is
    /// `0.047 * 255 = 11.985`, i.e. 12 to nearest and 11 truncated. Both are
    /// asserted where that happens, so neither the criterion nor the shipped
    /// cast is taken on trust.
    fn u8_nearest(color: Rgba) -> [u8; 3] {
        [(color.r * 255.0).round() as u8, (color.g * 255.0).round() as u8, (color.b * 255.0).round() as u8]
    }

    /// THE acceptance criterion for the whole lighting-model series: the
    /// Mapbox Standard dark style's `land` background, which renders
    /// near-white today (`rgb(245,241,240)`, `colorLand`'s
    /// `hsl(20, 20%, 95%)` straight through), must render essentially black
    /// at zoom 14 under the fixture's own shipped `night` + `monochrome`
    /// config.
    ///
    /// The arithmetic, all of it from the reference: `theme: monochrome`
    /// grades `colorLand` to a neutral `graded::LAND` (0.986) FIRST; then,
    /// under `night`, both lights are black at zero intensity, so
    /// `ground_radiance` is exactly `[0, 0, 0]`; `land`'s
    /// `background-emissive-strength` folds to 0.05 at zoom 14 (see
    /// `the_vendored_dark_fixtures_land_emissive_strength_resolves_across_zoom`);
    /// so `mix(0, 1, 0.05) = 0.05` and `0.986 * 0.05 * 255 = 12.57`.
    #[test]
    fn the_vendored_dark_fixtures_land_background_renders_dark_at_night() {
        let style = or_skip!(standard_fixture_under_preset("night"));
        let land = land_background_color(&style);

        assert_rgba_close(land.value_at(14), scale_rgb(graded::LAND, 0.05), "night + monochrome at zoom 14");
        assert_eq!(u8_nearest(land.value_at(14)), [13, 13, 13], "night + monochrome at zoom 14");
        assert_eq!(u8_written(land.value_at(14)), [12, 12, 12], "and as the renderer's truncating cast writes it");
        // One zoom lower the emissive strength is 0.1, not 0.05 -- twice as
        // bright, and still nothing like the unlit near-white.
        assert_eq!(u8_nearest(land.value_at(13)), [25, 25, 25], "night + monochrome at zoom 13");
        // Alpha is never touched by the lighting.
        assert_eq!(land.value_at(14).a, 1.0, "alpha must survive the lighting untouched");
    }

    /// The counterweight, so this proves a lighting MODEL rather than "a
    /// switch that makes things darker": the same background under `day`
    /// comes out slightly BRIGHTER than the unlit colour, because day's
    /// ground radiance exceeds 1.0.
    ///
    /// Independently computed from the reference formulas against the
    /// fixture's own `day` + `monochrome` lights (ambient white at 0.85,
    /// directional white at 0.2 at zoom 14, direction `[180, 20]`):
    /// `ground_radiance = 1.017070`, `land`'s emissive strength = 0.25
    /// (its `measure-light` ramp clamps to its top stop at a brightness of
    /// 0.85), so `mix(1.017070, 1, 0.25) = 1.012802` -- applied to the
    /// GRADED colour `graded::LAND` (0.985999), not to the authored
    /// `hsl(20, 20%, 95%)`, giving 0.998622 on every channel.
    ///
    /// The grade is what makes the three channels equal: unlit, `land` is a
    /// warm off-white `(0.96, 0.946667, 0.94)`; `monochrome` puts it on the
    /// neutral axis before the light ever touches it.
    #[test]
    fn the_vendored_dark_fixtures_land_background_renders_bright_by_day() {
        let style = or_skip!(standard_fixture_under_preset("day"));
        let lit = land_background_color(&style).value_at(14);

        let expected = [0.998_622, 0.998_622, 0.998_622];
        for (channel, (got, want)) in ["r", "g", "b"].iter().zip([lit.r, lit.g, lit.b].iter().zip(expected.iter())) {
            assert!((got - want).abs() < 1e-4, "day channel {channel}: got {got:.6}, want {want:.6}");
        }
        assert_eq!(u8_nearest(lit), [255, 255, 255]);
        assert_eq!(u8_written(lit), [254, 254, 254], "and as the renderer's truncating cast writes it");
        assert_eq!(lit.a, 1.0, "alpha must survive the lighting untouched");
    }

    /// `day` legitimately drives a channel past 1.0, and the compiled colour
    /// must carry it that way: the only clamp is the saturating cast at the
    /// final 8-bit write. At zoom 12 the fixture's directional intensity is
    /// 0.3 (its `interpolate` clamps below its z13 stop) and `land`'s
    /// emissive strength is 0.1, giving a factor of 1.052143 against the
    /// graded red channel of 0.985999 -- 1.037412, which must NOT have been
    /// clipped to 1.0 during compilation.
    #[test]
    fn lighting_leaves_a_channel_above_one_unclamped() {
        let style = or_skip!(standard_fixture_under_preset("day"));
        let lit = land_background_color(&style).value_at(12);

        assert!(lit.r > 1.0, "the red channel must stay unclamped, got {}", lit.r);
        assert!((lit.r - 1.037_412).abs() < 1e-4, "got {}", lit.r);
        // And it still saturates -- not wraps -- on the way to 8 bits.
        assert_eq!(u8_written(lit), [255, 255, 255], "saturates on the way to 8 bits, never wraps");
    }

    /// The trap the plan flagged. `value_at_pair(z)` hands the GPU a
    /// `_from`/`_to` bracket to blend across the fractional zoom, and those
    /// are two DIFFERENT zooms -- so each half has to be lit at its own
    /// zoom, not both at `z`'s radiance and emissive strength. Baking the
    /// lighting into a per-integer-zoom `ZoomValue` is what makes that true
    /// by construction; this pins it, because a wrong colour here shows up
    /// only while the camera is between integer zooms and no screenshot
    /// would catch it.
    ///
    /// `land` under `night` is the sharpest case in the fixture: its
    /// emissive strength halves between zoom 13 and 14 (0.1 -> 0.05), so the
    /// two halves of the pair at zoom 13 must differ by exactly a factor of
    /// two.
    #[test]
    fn each_half_of_a_lit_colour_pair_is_lit_at_its_own_zoom() {
        let style = or_skip!(standard_fixture_under_preset("night"));
        let land = land_background_color(&style);

        let (from, to) = land.value_at_pair(13);
        assert_eq!(u8_nearest(from), [25, 25, 25], "the `from` half must be lit at zoom 13");
        assert_eq!(u8_nearest(to), [13, 13, 13], "the `to` half must be lit at zoom 14, not 13");
        assert!((from.r - 2.0 * to.r).abs() < 1e-6, "0.1 vs 0.05 emissive: exactly a factor of two");
    }

    /// Every compiled fill/line colour carrying this id -- one JSON layer
    /// expands into one `StyleLayer` per feature-filtered case, so a layer
    /// with a data-driven paint property has several (`aeroway line` has
    /// two). At least one, or the id is wrong.
    fn layer_colors<'a>(style: &'a crate::style::Style, id: &str) -> Vec<&'a ZoomValue<Rgba>> {
        let found: Vec<_> = style
            .layers
            .iter()
            .filter_map(|l| match l {
                StyleLayer::Fill { id: layer_id, color, .. } | StyleLayer::Line { id: layer_id, color, .. }
                    if layer_id == id =>
                {
                    Some(color)
                }
                _ => None,
            })
            .collect();
        assert!(!found.is_empty(), "no compiled fill/line layer {id:?}");
        found
    }

    /// The colour of the ONE compiled fill or line layer with this id.
    ///
    /// Insists on exactly one, because a composed style can hold two layers
    /// with the same id: 24 of the Standard fixture's ids appear in BOTH the
    /// root document and the imported basemap (`tunnel-pedestrian`,
    /// `landuse`, `ferry`, ...), and a `find` would silently measure the
    /// imported one -- which is lit differently, since it carries an
    /// emissive strength the root's copy does not. Callers pick ids that are
    /// unique to the document they mean, and this makes that a checked
    /// property rather than an assumption.
    fn layer_color<'a>(style: &'a crate::style::Style, id: &str) -> &'a ZoomValue<Rgba> {
        let mut matches = style.layers.iter().filter_map(|l| match l {
            StyleLayer::Fill { id: layer_id, color, .. } | StyleLayer::Line { id: layer_id, color, .. }
                if layer_id == id =>
            {
                Some(color)
            }
            _ => None,
        });
        let color = matches.next().unwrap_or_else(|| panic!("no compiled fill/line layer {id:?}"));
        assert!(matches.next().is_none(), "layer id {id:?} is not unique in this compiled style");
        color
    }

    /// Lighting is scene-GLOBAL: `Style.mergeAll` hoists whichever fragment
    /// declared the lights onto the ROOT style
    /// (`src/style/style.ts:897-967`, Mapbox GL JS 3.15.0 -- its
    /// `forEachFragmentStyle` walk visits fragments before the style that
    /// imported them), and `painter.uploadCommonLightUniforms`
    /// (`src/render/painter.ts:1764-1774`) then uploads that one pair for
    /// every 2D program, whichever document the layer being drawn came
    /// from.
    ///
    /// So the Standard fixture's ROOT-document layers -- which are not in
    /// the import that declares the lights, and which carry no
    /// `*-emissive-strength` at all (spec default 0) -- must be lit by the
    /// imported basemap's lights too. Lighting only the imported layers left
    /// these four drawing at their authored near-white over a black
    /// basemap: bright patches scattered across a dark map. They are
    /// `unslotted`, i.e. composited ON TOP of the whole basemap, so there is
    /// nothing to hide them.
    #[test]
    fn the_root_documents_own_layers_are_lit_by_the_imported_basemaps_lights() {
        // All four are ids the imported basemap does NOT also use -- see
        // `layer_color`.
        const NEAR_WHITE_ROOT_LAYERS: [&str; 4] = ["aeroway", "aeroway line", "building", "building line"];

        let night = or_skip!(standard_fixture_under_preset("night"));
        for id in NEAR_WHITE_ROOT_LAYERS {
            for color in layer_colors(&night, id) {
                let lit = color.value_at(16);
                assert!(
                    lit.r < 1e-6 && lit.g < 1e-6 && lit.b < 1e-6,
                    "root layer {id:?} emits nothing (default emissive 0) into a scene with no light \
                     in it, so it must be black, not near-white -- got {lit:?}"
                );
            }
        }

        // The counterweight, so this is not merely "everything went black":
        // under `day` the same layers come back at essentially their
        // authored colour, times the day radiance. `building line`'s colour
        // ramp tops out at `#d9d9d9` from zoom 16, and with no emissive
        // strength its factor is the radiance itself -- 1.017070 at zoom 16,
        // the directional intensity ramp's last stop being zoom 14.
        let day = or_skip!(standard_fixture_under_preset("day"));
        let lit = layer_color(&day, "building line").value_at(16);
        assert_rgba_close(lit, scale_rgb(parse_css_color("#d9d9d9").unwrap(), 1.017_07), "building line by day");
    }

    /// The `["max", 0, ["-", lightness, 55]]` darkening branch of `water`'s
    /// real `fill-color`, pinned end to end -- through config resolution,
    /// `to-hsla` decomposition and layer compilation -- at a zoom where the
    /// lighting does NOT multiply it away.
    ///
    /// That needs a scene the shipped fixture never produces, and only one
    /// override reaches it -- and, since fix round 2 of the task-2 report
    /// replaced `measure_light_brightness`'s approximation with Mapbox's
    /// real `calculateLightsBrightness`, a DIFFERENT override than this
    /// test used before: brightness now reads BOTH lights (weighted by the
    /// W3C relative-luminance curve and, for the directional term, how
    /// close its direction is to straight up), exactly like ground radiance
    /// does -- so the two are no longer independent, and it is provably
    /// impossible to make ground radiance non-zero while keeping brightness
    /// AT EXACTLY the domain's `0` endpoint (every term that can make
    /// radiance non-zero is, by construction, a term that also makes
    /// brightness non-zero). The old override (directional white at
    /// intensity 1, brightness pinned to exactly 0 by the AMBIENT-only
    /// approximation) no longer reaches the pure darkened endpoint at all:
    /// it would now interpolate almost all the way to plain `colorWater`.
    ///
    /// So this keeps the fixture's own `night` ambient (black, intensity 0,
    /// contributing nothing to either brightness or radiance) and overrides
    /// the DIRECTIONAL light to a small grey (`#333333`, i.e. `v = 0.2`) at
    /// intensity `0.3`, straight up (`direction: [0, 0]`, so `dir.z == 1`
    /// exactly and every trig term collapses): brightness lands just inside
    /// the domain (`0.004966`, rounded per `measure_light_brightness`'s
    /// `toFixed(6)`), so `water`'s ramp genuinely BLENDS -- not clamps --
    /// between the darkened branch and plain `colorWater`, at a fraction
    /// `t = 0.004966 / 0.02 = 0.2483` computed the same way
    /// `interpolate_folded`'s `Interpolation::t_at` does for a linear
    /// curve, and `Lerp::lerp`s between the two branches' own `Rgba`s (RGB
    /// space, not HSL) the same way it does. Ground radiance is exactly
    /// `[v, v, v] * intensity^(1/2.2)` per channel with `dir.z == 1` and a
    /// black ambient (`ambient_directional_factor` collapses to exactly `1`
    /// too, at `NdotL == 1`) -- `0.2 * 0.3^(1/2.2) == 0.115707`.
    ///
    /// Zoom 13 vs 14 then shows the colour ramp itself -- `colorWater`
    /// straight through below 14 (the `["zoom"]` ramp's first stop, with no
    /// `measure-light` involved at all), the blended value at and above 14
    /// -- with one identical lighting factor on both sides.
    #[test]
    fn waters_darkening_branch_survives_compilation_under_a_light_that_reaches_it() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        let lights = root["imports"][0]["data"]["lights"].as_array_mut().expect("the fixture has lights");
        let directional = lights
            .iter_mut()
            .find(|light| light.get("id").and_then(Value::as_str) == Some("directional"))
            .expect("the fixture has a directional light");
        directional["properties"]["color"] = serde_json::json!("#333333");
        directional["properties"]["intensity"] = serde_json::json!(0.3);
        directional["properties"]["direction"] = serde_json::json!(["literal", [0, 0]]);
        let style = parse_and_compile(root.to_string().as_bytes()).expect("the fixture must compile");

        let water = layer_color(&style, "water");
        const RADIANCE: f32 = 0.115_706_52;
        assert_rgba_close(
            water.value_at(13),
            scale_rgb(graded::WATER, RADIANCE),
            "water z13: colorWater straight through",
        );
        // The blended (NOT clamped) z14 value: `Lerp::lerp` between the
        // darkened branch (hsl(0, 0%, 21%)) and plain colorWater
        // (hsl(0, 0%, 76%)) at `t = 0.2483`, per this test's own doc
        // comment -- graded, and only then scaled by the same `RADIANCE`.
        // The blend is still asserted here rather than folded into the
        // constant: `graded::WATER_DARKENED_BLEND` is the grade OF it, and
        // this checks the compiler reaches the same 0.346565 first.
        let darkened = parse_css_color("hsl(0, 0%, 21%)").unwrap();
        let plain = parse_css_color("hsl(0, 0%, 76%)").unwrap();
        let blended = crate::style::Lerp::lerp(&darkened, &plain, 0.2483);
        assert!((blended.r - 0.346_565).abs() < 1e-6, "the pre-grade blend, got {}", blended.r);
        assert_rgba_close(
            water.value_at(14),
            scale_rgb(graded::WATER_DARKENED_BLEND, RADIANCE),
            "water z14: blended, not clamped",
        );
    }

    /// A scene with `lights` that will not fold -- here only ONE of the two
    /// the reference requires (`painter.uploadCommonLightUniforms` computes
    /// `u_ground_radiance` only `if (directionalLight && ambientLight)`),
    /// which `expression::ground_radiance` refuses for that reason. The
    /// contract is that this degrades: every layer is drawn UNLIT, none is
    /// dropped, and the refusal is recorded once for the style rather than
    /// once per layer.
    #[test]
    fn lights_that_will_not_fold_leave_the_style_unlit_and_record_one_degradation() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8,
                "lights": [{ "id": "ambient", "type": "ambient", "properties": { "color": "#ffffff", "intensity": 1 } }],
                "layers": [
                    { "id": "imported-water", "type": "fill", "source-layer": "water", "paint": { "fill-color": "#ff8000" } }
                ] } }],
            "layers": [
                { "id": "root-building", "type": "fill", "source-layer": "building", "paint": { "fill-color": "#ff8000" } }
            ]
        }"##;
        let _ = take_degradations();
        let style = parse_and_compile(json).expect("unfoldable lights degrade rather than failing the style");
        let degraded = take_degradations();

        assert_eq!(degraded.len(), 1, "recorded once for the scene, not once per layer: {degraded:?}");
        assert!(degraded[0].contains("ground radiance"), "the notice says what refused: {}", degraded[0]);
        assert_eq!(style.layers.len(), 2, "both layers are kept, just unlit");
        for id in ["imported-water", "root-building"] {
            assert_eq!(layer_color(&style, id).value_at(10), parse_css_color("#ff8000").unwrap(), "{id}");
        }
    }

    /// A style whose `lights` carry the ids their author felt like giving
    /// them, rather than ids that happen to equal their `type`. Both are
    /// legal -- `validate_lights.ts` requires an `id` and a `type`, and only
    /// `type` is an enum -- and the reference discriminates on `type`
    /// throughout (`Style.mergeAll`, `src/style/style.ts:924-931`).
    ///
    /// This has to work end to end, because the scene's lights are chosen in
    /// one place (`merge_scene_lights`) and read back in another
    /// (`expression::find_light`). When those two disagreed, a style like
    /// this one merged its lights correctly and then found neither, so every
    /// colour compiled UNLIT and every `measure-light` degraded -- silently,
    /// because the vendored fixture happens to name its lights after their
    /// types.
    #[test]
    fn a_style_whose_lights_carry_non_standard_ids_still_lights_every_colour() {
        let json = serde_json::json!({
            "version": 8,
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "lights": [
                    { "id": "sky", "type": "ambient", "properties": { "color": "#000000", "intensity": 0 } },
                    { "id": "sun", "type": "directional", "properties": {
                        "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
                ],
                "layers": [{ "id": "imported-water", "type": "fill", "source-layer": "water",
                    "paint": { "fill-color": "#ffffff", "fill-emissive-strength": 0.5 } }]
            }}],
            "layers": [{ "id": "root-building", "type": "fill", "source-layer": "building",
                "paint": { "fill-color": "#ffffff", "fill-emissive-strength": 0.25 } }]
        });
        let style = parse_and_compile(json.to_string().as_bytes()).expect("the style must compile");

        // Radiance is exactly [0,0,0] (both lights black at zero intensity),
        // so `mix(radiance, 1, strength)` is the strength itself.
        assert_rgba_close(
            layer_color(&style, "imported-water").value_at(10),
            scale_rgb(parse_css_color("#ffffff").unwrap(), 0.5),
            "the light-declaring document's own layer",
        );
        assert_rgba_close(
            layer_color(&style, "root-building").value_at(10),
            scale_rgb(parse_css_color("#ffffff").unwrap(), 0.25),
            "the root document's layer",
        );
    }

    /// `measure-light` reads the same entries through the same `find_light`,
    /// so it has to survive non-standard ids too -- and it is the half that
    /// fails soundlessly, degrading a colour rather than refusing.
    #[test]
    fn measure_light_reads_a_light_named_anything_as_long_as_its_type_says_ambient() {
        let json = serde_json::json!({
            "version": 8,
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "lights": [
                    { "id": "sky", "type": "ambient", "properties": { "color": "#ffffff", "intensity": 1 } },
                    { "id": "sun", "type": "directional", "properties": {
                        "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
                ],
                // Picks the SECOND stop only if brightness folds to 0.5:
                // white ambient at intensity 1 and a dark directional give
                // (1 * 1 * 0 + 1 * 1) / 2 = 0.5 exactly.
                "layers": [{ "id": "imported-water", "type": "fill", "source-layer": "water",
                    "paint": {
                        "fill-color": ["interpolate", ["linear"], ["measure-light", "brightness"],
                            0.0, "#ff0000", 0.5, "#0000ff"],
                        "fill-emissive-strength": 1
                    } }]
            }}],
            "layers": []
        });
        let _ = take_degradations();
        let style = parse_and_compile(json.to_string().as_bytes()).expect("the style must compile");
        assert!(take_degradations().is_empty(), "measure-light must fold, not degrade");
        // Emissive strength 1 hands the colour through unlit, so this is the
        // `measure-light` result alone.
        assert_eq!(layer_color(&style, "imported-water").value_at(10), parse_css_color("#0000ff").unwrap());
    }

    /// `forEachFragmentStyle` (`src/style/style.ts:995-1005`) traverses each
    /// fragment BEFORE the style that imported it, and `mergeAll` keeps
    /// overwriting as it goes -- so the ROOT document is visited last and its
    /// own lights win over any import's. Pinned with two scenes that light
    /// the same colour differently: the import's lights would leave a white
    /// fill white, the root's black it out.
    #[test]
    fn the_root_documents_own_lights_win_over_an_imports() {
        let json = serde_json::json!({
            "version": 8,
            "lights": [
                { "id": "ambient", "type": "ambient", "properties": { "color": "#000000", "intensity": 0 } },
                { "id": "directional", "type": "directional", "properties": {
                    "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
            ],
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "lights": [
                    { "id": "ambient", "type": "ambient", "properties": { "color": "#ffffff", "intensity": 1 } },
                    { "id": "directional", "type": "directional", "properties": {
                        "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
                ],
                "layers": [{ "id": "imported-water", "type": "fill", "source-layer": "water",
                    "paint": { "fill-color": "#ffffff", "fill-emissive-strength": 0 } }]
            }}],
            "layers": []
        });
        let style = parse_and_compile(json.to_string().as_bytes()).expect("the style must compile");

        // The import's own lights would give a radiance of exactly [1,1,1]
        // (white ambient at full intensity), leaving the fill white. The
        // root's give [0,0,0].
        let lit = layer_color(&style, "imported-water").value_at(10);
        assert!(
            lit.r < 1e-6 && lit.g < 1e-6 && lit.b < 1e-6,
            "the root document's lights must win over the import's -- got {lit:?}"
        );
    }

    /// `mergeAll` merges the ambient and the directional INDEPENDENTLY: two
    /// `if`s over the same fragment walk, each keeping its own last winner
    /// (`src/style/style.ts:924-931`). So a scene can take one light from
    /// each document, and does not need either document to declare a
    /// complete pair.
    ///
    /// A pure red ambient makes both halves load-bearing: without the
    /// import's ambient there is no radiance to tint with, and without the
    /// root's directional `find_light` refuses the pair outright (the
    /// reference's own both-lights gate), leaving the fill white either way.
    #[test]
    fn a_scene_can_take_its_two_lights_from_two_different_documents() {
        let json = serde_json::json!({
            "version": 8,
            "lights": [{ "id": "directional", "type": "directional", "properties": {
                "color": "#000000", "intensity": 0, "direction": ["literal", [0, 90]] } }],
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "lights": [{ "id": "ambient", "type": "ambient", "properties": {
                    "color": "#ff0000", "intensity": 1 } }],
                "layers": [{ "id": "imported-water", "type": "fill", "source-layer": "water",
                    "paint": { "fill-color": "#ffffff", "fill-emissive-strength": 0 } }]
            }}],
            "layers": []
        });
        let style = parse_and_compile(json.to_string().as_bytes()).expect("the style must compile");

        let lit = layer_color(&style, "imported-water").value_at(10);
        assert!((lit.r - 1.0).abs() < 1e-6, "red channel, got {}", lit.r);
        assert!(lit.g.abs() < 1e-6 && lit.b.abs() < 1e-6, "tinted red by the merged ambient, got {lit:?}");
    }

    /// The one asymmetry the lighting model leaves behind, pinned so it is a
    /// known limit rather than a surprise: ground radiance is scene-global
    /// (merged across every document, folded once, carried as an evaluated
    /// table), while `["measure-light", "brightness"]` still re-folds the
    /// light's own expressions from the ASKING document's `lights`. A
    /// root-document layer has none -- see `parse_and_compile`'s comment at
    /// `scene_scope` for why it cannot simply be handed the import's.
    ///
    /// What matters is that the failure is loud. The colour degrades to the
    /// property's literal fallback, the layer survives, and a notice naming
    /// both is recorded -- never a silently wrong brightness. Meanwhile the
    /// same layer's colour is still LIT, since the radiance did reach it.
    #[test]
    fn a_root_layers_measure_light_degrades_loudly_rather_than_resolving() {
        let json = serde_json::json!({
            "version": 8,
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "lights": [
                    { "id": "ambient", "type": "ambient", "properties": { "color": "#ffffff", "intensity": 1 } },
                    { "id": "directional", "type": "directional", "properties": {
                        "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
                ],
                // The SAME expression, in the document that declares the
                // lights: this half must fold.
                "layers": [{ "id": "imported-water", "type": "fill", "source-layer": "water", "paint": {
                    "fill-color": ["hsl", ["measure-light", "brightness"], 50, 50],
                    "fill-emissive-strength": 1
                }}]
            }}],
            "layers": [{ "id": "root-building", "type": "fill", "source-layer": "building", "paint": {
                "fill-color": ["hsl", ["measure-light", "brightness"], 50, 50],
                "fill-emissive-strength": 1
            }}]
        });
        let _ = take_degradations();
        let style = parse_and_compile(json.to_string().as_bytes()).expect("the style must compile");
        let degraded = take_degradations();

        assert_eq!(degraded.len(), 1, "exactly one notice, naming what could not fold: {degraded:?}");
        assert!(degraded[0].contains("root-building"), "the notice names the layer: {}", degraded[0]);
        assert!(degraded[0].contains("fill-color"), "the notice names the property: {}", degraded[0]);
        assert_eq!(style.layers.len(), 2, "the layer survives the degradation");
        // `or_degrade`'s literal fallback for a colour is opaque black --
        // and it is still put through the lighting, which the emissive
        // strength of 1 then hands back unchanged.
        let fallback = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
        assert_eq!(layer_color(&style, "root-building").value_at(10), fallback);
        // And the same expression one document down, where the `lights` it
        // asks about live, folds: brightness is (0 + 1 * 1) / 2 = 0.5, so
        // the colour is `hsl(0.5, 50%, 50%)`.
        assert_eq!(
            layer_color(&style, "imported-water").value_at(10),
            parse_css_color("hsl(0.5, 50%, 50%)").unwrap(),
            "the asymmetry is about which DOCUMENT is asking, not about the expression"
        );
    }

    /// Every `ZoomValue<Rgba>` a compiled layer carries, named exhaustively.
    ///
    /// # Why the patterns spell out every field instead of using `..`
    ///
    /// This is the structural half of the guard that no colour can ship
    /// unlit. `light_ground_color` is applied at seven hand-written call
    /// sites, one per colour field, and nothing about the code stops an
    /// eighth colour -- a `fill-outline-color`, a `Circle` variant, an
    /// icon-halo split -- from being added without one: it would draw at
    /// full brightness on a dark map and every existing test would still
    /// pass. That is this plan's own original bug, one level down.
    ///
    /// So: no `..` here. Adding ANY field to any `StyleLayer` variant is a
    /// compile error at this function until the author says what it is, and
    /// `every_colour_a_compiled_layer_carries_is_lit` then fails if a field
    /// classified as a colour is not lit. The compiler forces the question;
    /// the test answers it. A non-colour field costs one `_` here, which is
    /// the price of the guarantee.
    #[rustfmt::skip]
    fn colours_of(layer: &StyleLayer) -> Vec<(&'static str, &ZoomValue<Rgba>)> {
        match layer {
            StyleLayer::Background { color, id: _, min_zoom: _, max_zoom: _ } => vec![("background-color", color)],
            StyleLayer::Fill { color, outline_color, id: _, source_layer: _, filter: _, opacity: _, min_zoom: _, max_zoom: _ } => {
                // The eighth colour this function's doc comment predicted,
                // now real. `outline_color` is `None` on every layer that
                // does not declare `fill-outline-color`, and a layer that
                // does declare one must have it lit exactly like the fill.
                let mut colours = vec![("fill-color", color)];
                if let Some(outline_color) = outline_color {
                    colours.push(("fill-outline-color", outline_color));
                }
                colours
            }
            // `light.color` is not a paint colour: it is the light the
            // walls are shaded BY, and lighting it through the scene's
            // radiance would light the light.
            StyleLayer::Extrusion {
                color,
                id: _, source_layer: _, filter: _, group: _, opacity: _, height: _, base: _, vertical_gradient: _,
                translate: _, translate_anchor: _, light: _, min_zoom: _, max_zoom: _,
            } => vec![("fill-extrusion-color", color)],
            StyleLayer::Line {
                color,
                id: _, source_layer: _, filter: _, width: _, opacity: _, gap_width: _, cap: _, join: _,
                dash: _, min_zoom: _, max_zoom: _,
            } => vec![("line-color", color)],
            StyleLayer::Symbol {
                text_color, text_halo_color, icon_color, icon_halo_color,
                id: _, source_layer: _, filter: _, text_field: _, icon_image: _, text_size: _,
                text_offset_x: _, text_offset_y: _, text_radial_offset: _, text_anchor: _, text_max_width: _, icon_size: _,
                symbol_sort_key: _, text_halo_width: _,
                text_font: _,
                text_halo_blur: _,
                icon_halo_width: _, placement: _, spacing_px: _,
                icon_opacity: _, text_opacity: _,
                icon_rotate_deg: _, rotation_alignment: _, text_rotation_alignment: _, icon_text_fit: _, bypass_collision: _,
                min_zoom: _, max_zoom: _,
            } => vec![
                ("text-color", text_color),
                ("text-halo-color", text_halo_color),
                ("icon-color", icon_color),
                ("icon-halo-color", icon_halo_color),
            ],
            // **A raster layer has no colour at all**, and that is a
            // statement about the layer type, not a gap in this list. Its
            // appearance is the texel it samples; the only paint property
            // it carries is `raster-opacity`, a scalar. There is nothing
            // here for the lighting model to light -- which is exactly
            // right: imagery is already a photograph of lit ground, and
            // multiplying it by a light would light it twice.
            //
            // Spelled out rather than swept into a `_` arm so that a
            // future variant that DOES carry a colour cannot slip past
            // this function silently. That is the property its doc comment
            // claims and the reason it destructures every field by name.
            StyleLayer::Raster { id: _, source: _, opacity: _, min_zoom: _, max_zoom: _ } => vec![],
            StyleLayer::Circle { paint, id: _, source_layer: _, filter: _, min_zoom: _, max_zoom: _ } => {
                let crate::style::CirclePaint {
                    color, stroke_color,
                    radius: _, opacity: _, stroke_width: _, stroke_opacity: _, blur: _, pitch: _,
                } = paint;
                vec![("circle-color", color), ("circle-stroke-color", stroke_color)]
            }
        }
    }

    /// The behavioural half of that guard: under a scene whose ground
    /// radiance is exactly `[0, 0, 0]`, EVERY colour of EVERY compiled layer
    /// must come out black when its emissive strength is 0 -- because
    /// `mix(0, 1, 0) = 0`. A colour that never reached `light_ground_color`
    /// would come through as the white it was authored as, and this fails
    /// naming it.
    ///
    /// Every colour property this compiler reads is set here, so the check
    /// covers all four layer types at once; `colours_of` is what makes it
    /// exhaustive rather than a list someone has to remember to extend.
    #[test]
    fn every_colour_a_compiled_layer_carries_is_lit() {
        let json = serde_json::json!({
            "version": 8,
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "lights": [
                    { "id": "ambient", "type": "ambient", "properties": { "color": "#000000", "intensity": 0 } },
                    { "id": "directional", "type": "directional", "properties": {
                        "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
                ],
                "layers": [
                    { "id": "land", "type": "background", "paint": {
                        "background-color": "#ffffff", "background-emissive-strength": 0 } },
                    { "id": "water", "type": "fill", "source-layer": "water", "paint": {
                        "fill-color": "#ffffff", "fill-emissive-strength": 0 } },
                    { "id": "road", "type": "line", "source-layer": "road", "paint": {
                        "line-color": "#ffffff", "line-emissive-strength": 0 } },
                    { "id": "poi", "type": "symbol", "source-layer": "poi",
                        "layout": { "text-field": "{name}" },
                        "paint": {
                            "text-color": "#ffffff", "text-halo-color": "#ffffff",
                            "icon-color": "#ffffff", "icon-halo-color": "#ffffff",
                            "text-emissive-strength": 0, "icon-emissive-strength": 0
                        } }
                ]
            }}],
            "layers": []
        });
        let style = parse_and_compile(json.to_string().as_bytes()).expect("the style must compile");
        assert_eq!(style.layers.len(), 4, "one compiled layer per authored layer: {:?}", layer_ids(&style));

        let mut checked = 0;
        for layer in &style.layers {
            for (property, colour) in colours_of(layer) {
                for zoom in [0, 10, 22] {
                    let lit = colour.value_at(zoom);
                    assert!(
                        lit.r < 1e-6 && lit.g < 1e-6 && lit.b < 1e-6,
                        "{property} was not lit: it emits nothing into a scene with no light in it, \
                         so it must be black at zoom {zoom} -- got {lit:?}"
                    );
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 7, "all seven of this compiler's colour properties must be covered");
    }

    /// A synthetic `lights` array whose ground radiance is exactly
    /// `[0, 0, 0]` (both lights black at zero intensity), which reduces
    /// `mix(radiance, 1, e)` to `e` and makes every expected value
    /// arithmetic anyone can do in their head.
    /// A scope that lights colours the way `parse_and_compile` lights a whole
    /// style: the scene's radiance folded ONCE from `lights`, then carried as
    /// an evaluated table. `table` is the caller's storage for it, because a
    /// `Scope` borrows the table rather than owning it -- the same reason
    /// `parse_and_compile` keeps it in a local.
    fn lit_scope<'a>(lights: &'a Value, table: &'a mut Vec<[f32; 3]>) -> Scope<'a> {
        *table = scene_radiance_table(Scope::root().with_lights(lights)).expect("these lights must fold");
        let table: &'a [[f32; 3]] = table;
        Scope::root().with_ground_radiance(table)
    }

    fn black_lights() -> Value {
        serde_json::json!([
            { "id": "ambient", "properties": { "color": "#000000", "intensity": 0 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]] } }
        ])
    }

    /// Fill and line colours are lit by their own `*-emissive-strength`,
    /// with the two endpoints of the `mix` pinned: an emissive strength of
    /// 0 hands the colour entirely to the scene light (black here), and one
    /// of 1 leaves it exactly as authored.
    #[test]
    fn fill_and_line_colours_are_lit_by_their_own_emissive_strength() {
        let lights = black_lights();
        let mut radiance = Vec::new();
        let scope = lit_scope(&lights, &mut radiance);

        for (strength, expected) in [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0)] {
            let fill = serde_json::json!({
                "id": "f", "type": "fill", "source-layer": "s",
                "paint": { "fill-color": "#ff0000", "fill-emissive-strength": strength }
            });
            let compiled = compile_layer("f", "fill", &fill, scope).expect("the layer must survive");
            let StyleLayer::Fill { color, .. } = &compiled[0] else { panic!("expected a fill layer") };
            assert!(
                (color.value_at(10).r - expected).abs() < 1e-6,
                "fill at emissive {strength}: got {}, want {expected}",
                color.value_at(10).r
            );

            let line = serde_json::json!({
                "id": "l", "type": "line", "source-layer": "s",
                "paint": { "line-color": "#ff0000", "line-emissive-strength": strength }
            });
            let compiled = compile_layer("l", "line", &line, scope).expect("the layer must survive");
            let StyleLayer::Line { color, .. } = &compiled[0] else { panic!("expected a line layer") };
            assert!(
                (color.value_at(10).r - expected).abs() < 1e-6,
                "line at emissive {strength}: got {}, want {expected}",
                color.value_at(10).r
            );
        }
    }

    /// `symbolFrag` has exactly ONE `emissive_strength` attribute and calls
    /// `apply_lighting_with_emission_ground` on whichever colour the pass
    /// ended up with, so `text-halo-color` rides `text-emissive-strength`
    /// and `icon-halo-color` rides `icon-emissive-strength`. All four
    /// symbol colours are lit, each by its own side's strength.
    #[test]
    fn every_symbol_colour_is_lit_by_its_own_sides_emissive_strength() {
        let lights = black_lights();
        let mut radiance = Vec::new();
        let scope = lit_scope(&lights, &mut radiance);
        let json = serde_json::json!({
            "id": "poi", "type": "symbol", "source-layer": "poi",
            "layout": { "text-field": "{name}" },
            "paint": {
                "text-color": "#ff0000", "text-halo-color": "#00ff00",
                "icon-color": "#0000ff", "icon-halo-color": "#ffffff",
                "text-emissive-strength": 0.25, "icon-emissive-strength": 0.5
            }
        });
        let compiled = compile_symbol_layer(&json, scope).expect("the layer must survive");
        let StyleLayer::Symbol { text_color, text_halo_color, icon_color, icon_halo_color, .. } = &compiled[0] else {
            panic!("expected a symbol layer")
        };

        assert!((text_color.value_at(10).r - 0.25).abs() < 1e-6, "text-color rides text-emissive-strength");
        assert!((text_halo_color.value_at(10).g - 0.25).abs() < 1e-6, "text-halo-color rides text-emissive-strength");
        assert!((icon_color.value_at(10).b - 0.5).abs() < 1e-6, "icon-color rides icon-emissive-strength");
        assert!((icon_halo_color.value_at(10).r - 0.5).abs() < 1e-6, "icon-halo-color rides icon-emissive-strength");
    }

    /// `u_ground_radiance` is a vec3, not a scalar: a coloured light TINTS
    /// the map rather than merely dimming it. A pure red ambient light at
    /// full intensity (with the directional light dark, so it contributes
    /// nothing) gives a radiance of exactly `[1, 0, 0]`, which turns a
    /// half-transparent white fill pure red -- and leaves its alpha alone.
    #[test]
    fn a_coloured_light_tints_per_channel_and_never_touches_alpha() {
        let lights = serde_json::json!([
            { "id": "ambient", "properties": { "color": "#ff0000", "intensity": 1 } },
            { "id": "directional", "properties": {
                "color": "#000000", "intensity": 0, "direction": ["literal", [0, 90]] } }
        ]);
        let mut radiance = Vec::new();
        let scope = lit_scope(&lights, &mut radiance);
        let json = serde_json::json!({
            "id": "f", "type": "fill", "source-layer": "s",
            "paint": { "fill-color": "rgba(255, 255, 255, 0.5)", "fill-emissive-strength": 0 }
        });
        let compiled = compile_layer("f", "fill", &json, scope).expect("the layer must survive");
        let StyleLayer::Fill { color, .. } = &compiled[0] else { panic!("expected a fill layer") };

        let lit = color.value_at(10);
        assert!((lit.r - 1.0).abs() < 1e-6, "red channel, got {}", lit.r);
        assert!(lit.g.abs() < 1e-6, "green channel, got {}", lit.g);
        assert!(lit.b.abs() < 1e-6, "blue channel, got {}", lit.b);
        assert!((lit.a - 0.5).abs() < 1e-6, "alpha is never lit, got {}", lit.a);
    }

    /// The gate that keeps both vendored styles byte-identical (and is what
    /// `both_vendored_styles_compile_to_a_pinned_fingerprint` proves at
    /// whole-style scale): no `lights` on the scope means no lighting at
    /// all, so a colour comes through exactly as authored even though its
    /// emissive strength is 0 -- which, WITH lights, would have blacked it
    /// out entirely.
    #[test]
    fn a_style_with_no_lights_leaves_every_colour_exactly_as_authored() {
        let json = serde_json::json!({
            "id": "f", "type": "fill", "source-layer": "s",
            "paint": { "fill-color": "#ff8000", "fill-emissive-strength": 0 }
        });
        let compiled = compile_layer("f", "fill", &json, Scope::root()).expect("the layer must survive");
        let StyleLayer::Fill { color, .. } = &compiled[0] else { panic!("expected a fill layer") };

        assert_eq!(color.value_at(10), parse_css_color("#ff8000").unwrap());
        assert!(color.is_constant(), "an unlit constant colour must stay a single-stop constant");
    }

    /// The cross-product pairing, on the one real shape that exercises it:
    /// `roads`' `line-emissive-strength` splits into two class-filtered
    /// layers (0.3 for motorway/trunk, 0.2 for everything else), and each
    /// resulting layer's colour must be lit with ITS OWN strength, not with
    /// the other branch's. With no light in the scene, white roads come out
    /// as two distinguishable greys -- which is the whole point: a
    /// `#ffffff` road on a black map has to stay legible without being
    /// white.
    #[test]
    fn each_filtered_layer_is_lit_with_its_own_feature_dependent_strength() {
        let json = serde_json::json!({
            "id": "roads", "type": "line", "source-layer": "road",
            "paint": {
                "line-color": "#ffffff",
                "line-emissive-strength": ["let",
                    "brightness", ["config", "roadsBrightness"],
                    "isHighClass", ["match", ["get", "class"], ["motorway", "trunk"], true, false],
                    ["-", ["var", "brightness"], ["case", ["var", "isHighClass"], 0.1, 0.2]]]
            }
        });
        let config = ResolvedConfig::from_pairs([("roadsBrightness", serde_json::json!(0.4))]);
        let lights = black_lights();
        let mut radiance = Vec::new();
        let scope = lit_scope(&lights, &mut radiance).with_config(&config);
        let layers = compile_layer("roads", "line", &json, scope).expect("the layer must survive");
        assert_eq!(layers.len(), 2, "one layer per class-based case: {layers:?}");

        for layer in &layers {
            let StyleLayer::Line { filter, color, .. } = layer else { panic!("expected a line layer") };
            let expected = match filter {
                Filter::In { .. } => 0.3,
                Filter::Not(_) => 0.2,
                other => panic!("unexpected filter: {other:?}"),
            };
            assert_rgba_close(
                color.value_at(10),
                scale_rgb(parse_css_color("#ffffff").unwrap(), expected),
                &format!("roads {filter:?}"),
            );
        }
    }

    /// A `step` colour's hard cutoff must survive the lighting bake even
    /// though the lighting factor moves with zoom -- which is the normal
    /// case in a lit style, not a corner one: the Standard document's
    /// directional intensity and most of its emissive strengths are zoom
    /// ramps.
    ///
    /// This is the representational conflict `light_ground_color` documents,
    /// pinned on the side it takes. A `ZoomValue` cannot be discontinuous at
    /// one zoom and continuous everywhere else, so the author's cutoff wins:
    /// the baked value stays stepped, `value_at_pair` keeps handing the GPU
    /// a no-op bracket, and the camera never sees a colour halfway between
    /// the two branches that the style never declared. What is given up is
    /// that the lighting itself only moves at integer zooms here -- the same
    /// approximation the `step` already carried before any lighting existed.
    ///
    /// The counterweight is
    /// `each_half_of_a_lit_colour_pair_is_lit_at_its_own_zoom`: a colour
    /// with no cutoffs to protect DOES gain the blend, so the light stays
    /// continuous wherever it can.
    #[test]
    fn a_stepped_colours_hard_cutoff_survives_a_varying_lighting_factor() {
        let lights = black_lights();
        let mut radiance = Vec::new();
        let scope = lit_scope(&lights, &mut radiance);
        let json = serde_json::json!({
            "id": "f", "type": "fill", "source-layer": "s",
            "paint": {
                "fill-color": ["step", ["zoom"], "#ff0000", 10, "#00ff00"],
                // Radiance is zero here, so the factor IS this: 0.9 at zoom
                // 9, 1.0 at zoom 10 and above.
                "fill-emissive-strength": ["interpolate", ["linear"], ["zoom"], 0, 0.0, 10, 1.0]
            }
        });
        let compiled = compile_layer("f", "fill", &json, scope).expect("the layer must survive");
        let StyleLayer::Fill { color, .. } = &compiled[0] else { panic!("expected a fill layer") };

        assert!(!color.is_interpolated(), "a step's cutoff must not become a gradient");
        assert_rgba_close(color.value_at(9), scale_rgb(parse_css_color("#ff0000").unwrap(), 0.9), "below the cutoff");
        assert_rgba_close(color.value_at(10), scale_rgb(parse_css_color("#00ff00").unwrap(), 1.0), "at the cutoff");
        let (from, to) = color.value_at_pair(9);
        assert_eq!(from, to, "the bracket the GPU blends across the cutoff must stay a no-op pair");
    }

    /// The counterweight: a construct the folder does NOT recognize keeps
    /// today's behaviour, a loud per-layer skip. `water-depth` in the
    /// vendored Mapbox Streets style is exactly this and must keep being
    /// skipped.
    #[test]
    fn an_unrecognized_fill_color_expression_still_drops_the_layer() {
        let json = serde_json::json!({
            "id": "water-depth", "type": "fill", "source-layer": "depth",
            "paint": { "fill-color": ["interpolate", ["linear"], ["get", "min_depth"],
                0, "hsla(200, 100%, 80%, 0.35)", 200, "hsla(200, 100%, 72%, 0.35)"] }
        });
        let layers = compile_layer("water-depth", "fill", &json, Scope::root()).expect("the skip is soft");
        assert!(layers.is_empty(), "an unsupported construct still drops the layer");
    }


    #[test]
    fn an_unfoldable_symbol_text_color_degrades_and_keeps_the_layer() {
        let json = serde_json::json!({
            "id": "poi-label", "type": "symbol", "source-layer": "poi",
            "layout": { "text-field": ["get", "name"], "text-size": 12 },
            "paint": { "text-color": ["hsl", ["config", "hue"], 20, 95] }
        });
        let layers = compile_symbol_layer(&json, Scope::root()).expect("the layer must survive");
        assert_eq!(layers.len(), 1);
        match &layers[0] {
            StyleLayer::Symbol { text_color, .. } => {
                assert_eq!(text_color.value_at(10), Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 })
            }
            other => panic!("expected a symbol layer, got {other:?}"),
        }
    }

    /// The whole colour-decomposition idiom as a real fill layer, with a
    /// literal standing in for the `config` the next commit resolves.
    #[test]
    fn the_standard_colour_decomposition_compiles_as_a_real_fill_layer() {
        let json = serde_json::json!({
            "id": "land", "type": "fill", "source-layer": "landuse",
            "paint": { "fill-color": ["let",
                "h_colorLand", ["at", 0, ["to-hsla", "hsl(20, 20%, 95%)"]],
                "s_colorLand", ["at", 1, ["to-hsla", "hsl(20, 20%, 95%)"]],
                "l_colorLand", ["at", 2, ["to-hsla", "hsl(20, 20%, 95%)"]],
                ["hsl", ["var", "h_colorLand"], ["var", "s_colorLand"], ["var", "l_colorLand"]]] }
        });
        let layers = compile_layer("land", "fill", &json, Scope::root()).expect("the layer must compile");
        assert_eq!(layers.len(), 1);
        let folded = fill_color_at(&layers[0], 10);
        let direct = parse_css_color("hsl(20, 20%, 95%)").unwrap();
        assert!(
            (folded.r - direct.r).abs() < 1e-6 && (folded.g - direct.g).abs() < 1e-6 && (folded.b - direct.b).abs() < 1e-6,
            "folded {folded:?} != direct {direct:?}"
        );
    }

    // End-to-end check that line-placed symbol icons (the one-way arrows)
    // actually compile out of the two real vendored styles, not just out of
    // hand-built JSON fixtures.

    #[test]
    fn every_one_way_arrow_layer_in_both_vendored_styles_compiles() {
        for (path, expected) in [
            ("examples/liberty-style.json", vec!["road_one_way_arrow", "road_one_way_arrow_opposite"]),
            (
                "examples/mapbox-streets-style.json",
                vec![
                    "tunnel-oneway-arrow-blue",
                    "tunnel-oneway-arrow-white",
                    "road-oneway-arrow-blue",
                    "road-oneway-arrow-white",
                    "bridge-oneway-arrow-blue",
                    "bridge-oneway-arrow-white",
                ],
            ),
        ] {
            let bytes = if path.contains("mapbox-") {
                let Some(bytes) = crate::mapbox_fixtures::read(path) else { continue };
                bytes
            } else {
                std::fs::read(path).expect("run from the repo root")
            };
            let style = parse_and_compile(&bytes).expect("vendored style must compile");
            for id in expected {
                assert!(
                    style.layers.iter().any(|l| matches!(l, StyleLayer::Symbol { id: layer_id, .. } if layer_id == id)),
                    "{path}: {id} must compile now that line placement is supported"
                );
            }
        }
    }

    /// Replaces `0x<hex>` runs (a `ZoomValue`'s `interpolator` function
    /// pointer, printed by its derived `Debug`) with a fixed placeholder.
    /// Needed because that address moves under ASLR between runs even when
    /// nothing about the compiled `Style` changed -- see `ZoomValue`'s
    /// `interpolator` field doc comment.
    fn normalize_addresses(s: &str) -> String {
        let chars: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '0' && chars.get(i + 1) == Some(&'x') {
                let mut j = i + 2;
                while j < chars.len() && chars[j].is_ascii_hexdigit() {
                    j += 1;
                }
                if j > i + 2 {
                    out.push_str("0xADDR");
                    i = j;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    /// A cheap fingerprint of exactly what the manual byte-identity check
    /// this task's author ran (a throwaway `examples/dump_styles.rs`,
    /// diffing `{:#?}` of `parse_and_compile` against the base commit) was
    /// standing in for: address-normalized `{:#?}` of the compiled `Style`,
    /// hashed rather than checked in whole (the un-normalized dump for both
    /// vendored styles runs well over a megabyte).
    fn style_fingerprint(style: &Style) -> u64 {
        let debug = format!("{style:#?}");
        // `Style::sky` is cut back out, so every number pinned before the
        // field existed still means what it did: `sky` is the LAST field,
        // and neither vendored style declares one (pinned separately by
        // `a_style_without_a_sky_gets_the_spec_defaults`).
        let debug = match debug.rfind("\n    sky: Sky {") {
            Some(at) => format!("{}\n}}", &debug[..at]),
            None => debug,
        };
        // `text_radial_offset: None` is cut back out for the same reason:
        // the field is newer than every pinned number, and a layer that
        // declares no radial offset compiles to what it did before it.
        let debug: String = debug
            .lines()
            .filter(|line| line.trim() != "text_radial_offset: None," && line.trim() != "off_grid: None,")
            .collect::<Vec<_>>()
            .join("\n");
        // And the default `icon_text_fit` (`none`, no padding), for the same
        // reason: newer than every pinned number, and a layer that declares
        // no `icon-text-fit` compiles to what it did before it.
        let default_fit = [
            "icon_text_fit: IconTextFit {",
            "fit: None,",
            "padding: [",
            "0.0,",
            "0.0,",
            "0.0,",
            "0.0,",
            "],",
            "},",
        ];
        let lines: Vec<&str> = debug.lines().collect();
        let mut kept = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            let block = lines.get(i..i + default_fit.len());
            if block.is_some_and(|b| b.iter().zip(default_fit).all(|(l, want)| l.trim() == want)) {
                i += default_fit.len();
                continue;
            }
            kept.push(lines[i]);
            i += 1;
        }
        let debug = kept.join("\n");
        let normalized = normalize_addresses(&debug);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&normalized, &mut hasher);
        std::hash::Hasher::finish(&hasher)
    }

    /// A digest of every COLOUR in a compiled style, and nothing else: each
    /// layer's id, each `ZoomValue<Rgba>` field's name, whether it blends,
    /// and the exact bit pattern of every channel of every stop.
    ///
    /// Deliberately not `style_fingerprint`, which hashes `{:#?}` of the
    /// whole `Style` and therefore moves whenever any field is ADDED --
    /// which is exactly what happened at `4021d15`, when `StyleLayer` gained
    /// `emissive_strength` and both fingerprints legitimately changed
    /// without a single colour moving. That re-pin destroyed the only
    /// evidence that the colours had not moved. This digest cannot be
    /// disturbed by a field it does not read.
    fn colour_digest(style: &Style) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for layer in &style.layers {
            let (id, colours): (&str, Vec<(&str, &ZoomValue<Rgba>)>) = match layer {
                StyleLayer::Background { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Fill { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Extrusion { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Line { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Symbol { id, text_color, text_halo_color, icon_color, icon_halo_color, .. } => (
                    id,
                    vec![
                        ("text_color", text_color),
                        ("text_halo_color", text_halo_color),
                        ("icon_color", icon_color),
                        ("icon_halo_color", icon_halo_color),
                    ],
                ),
                // **Skipped entirely, not hashed with an empty list.**
                // This fingerprint's promise is "every COLOUR is
                // unchanged", and a raster layer has none -- its
                // appearance is its texels. Hashing its id anyway would
                // move the fingerprint when a raster layer was added to a
                // style, which is exactly what happened: Liberty's
                // `natural_earth` started compiling and this test fired
                // "a PRE-EXISTING colour moved", naming a colour that had
                // not moved at all. Skipping keeps the number a statement
                // about colours.
                StyleLayer::Raster { .. } => continue,
                StyleLayer::Circle { id, paint, .. } => (id, vec![("color", &paint.color), ("stroke_color", &paint.stroke_color)]),
            };
            id.hash(&mut hasher);
            for (field, value) in colours {
                field.hash(&mut hasher);
                // `interpolator` is a private function POINTER, so its
                // presence is read off the derived `Debug` rather than
                // compared by address -- comparing addresses is what forced
                // `style_fingerprint` to normalise them in the first place.
                // It matters here because a colour that gained a blend
                // gained a colour: `value_at_pair` would hand the GPU two
                // different endpoints where it used to hand it one twice.
                format!("{value:?}").contains("interpolator: Some").hash(&mut hasher);
                for &(zoom, colour) in &value.stops {
                    zoom.hash(&mut hasher);
                    for channel in [colour.r, colour.g, colour.b, colour.a] {
                        channel.to_bits().hash(&mut hasher);
                    }
                }
            }
        }
        hasher.finish()
    }

    /// The lighting model's central promise, kept as a standing artefact
    /// rather than an argument: a style with no `lights` gets no lighting,
    /// so every colour Liberty and Mapbox Streets compile to is BIT-identical
    /// to what it was before any of this existed.
    ///
    /// # Where these two numbers came from
    ///
    /// Not from running this test and writing down what it printed. They were
    /// computed at commit `4a2101c` -- the last commit before the
    /// lighting-model branch, when `light_ground_color` did not exist -- by
    /// checking out `src/` at that revision, appending `colour_digest` above
    /// verbatim (it reads only fields that existed then, and still do), and
    /// printing it for both fixtures. So this test compares today's colours
    /// against the ones the renderer actually shipped before the branch.
    ///
    /// # Therefore: do NOT re-pin these
    ///
    /// `both_vendored_styles_compile_to_a_pinned_fingerprint` says to update
    /// its constants once you have confirmed the diff is the change you
    /// meant. That is right for a whole-`Style` `Debug` hash, which moves for
    /// harmless reasons like a new field. It is WRONG here. These two numbers
    /// are archaeological: they record what a specific past revision
    /// produced, and nothing in the present can make them a different pair of
    /// numbers. If this test fails, a colour in a style with no lights
    /// changed -- which is the one thing the lighting model promised could
    /// not happen. Find the colour.
    ///
    /// # The one legitimate way this digest may move: a layer ADDED
    ///
    /// The rule above is about a colour MOVING. A colour ARRIVING is a
    /// different event, and the digest cannot tell the two apart on its own:
    /// it walks `style.layers` in order, so a new layer shifts the hash
    /// exactly as a changed one would. The fill-extrusion fallback added
    /// one. Liberty's `building-3d` used to be skipped for its layer type
    /// and now compiles to a flat footprint, so Liberty's digest covers 108
    /// layers where it covered 107.
    ///
    /// Rather than overwrite the archaeological number and destroy the
    /// evidence -- the exact mistake this test's own doc comment was written
    /// to stop -- the assertion is SPLIT. The 4a2101c number is kept and
    /// still asserted, against the style with `building-3d` removed; the new
    /// number is asserted separately, against the whole style. The first
    /// assertion carries the original promise unchanged and is still
    /// comparing against a value computed at 4a2101c.
    ///
    /// That the split is honest was confirmed mechanically before it was
    /// written, not assumed: with `building-3d` dropped from
    /// `style.layers`, `colour_digest` came back 12340372562122989070 --
    /// bit for bit the value pinned here since 4a2101c -- so not one
    /// previously-compiling colour moved.
    ///
    /// Mapbox Streets needed the same split later, when
    /// `LayoutValue::ZoomOfMatchStep` let `road-label`, `path-pedestrian-
    /// label` and `settlement-subdivision-label` compile (see the
    /// `MAPBOX_STREETS_COLOURS_WITH_MATCH_SHAPED_LABELS` assertion below and
    /// its own comment for the same mechanical confirmation).
    #[test]
    fn no_lights_no_lighting_every_vendored_colour_is_unchanged_since_before_the_branch() {
        // Printed by `colour_digest` at 4a2101c. See this test's doc comment.
        const LIBERTY_COLOURS_AT_4A2101C: u64 = 12_340_372_562_122_989_070;
        const MAPBOX_STREETS_COLOURS_AT_4A2101C: u64 = 16_837_440_352_253_820_035;
        // Those same 107 layers, plus `building-3d`'s flat footprint.
        const LIBERTY_COLOURS_WITH_FLAT_BUILDING_3D: u64 = 16_480_228_326_987_495_974;
        // The 4a2101c layers, plus `road-label` (x2), `path-pedestrian-label`
        // and `settlement-subdivision-label` -- see the split assertion
        // below for how this was confirmed rather than guessed.
        // ...plus the twelve `StyleLayer`s of the dropped-label-layers change
        // (`natural-line-label`, `water-line-label` x2, `water-point-label`
        // x2, `transit-label` x6, `airport-label`); the split assertion
        // below strips them and gets the 4a2101c digest back.
        const MAPBOX_STREETS_COLOURS_WITH_DROPPED_LABELS: u64 = 18_013_902_420_076_898_086;

        // THE archaeological assertion: strip the one layer this change
        // added, and every colour that remains is the one 4a2101c produced.
        let mut liberty_without_the_new_layer = liberty();
        // By id, not variant: `building-3d` was a flat `Fill` when this was
        // written and is a `StyleLayer::Extrusion` now (3D building
        // extrusion, Task 4). `colour_digest` hashes an extrusion's colour
        // exactly as it hashed that fill's, so neither number moved.
        liberty_without_the_new_layer.layers.retain(|layer| layer.id() != "building-3d");
        assert_eq!(
            liberty_without_the_new_layer.layers.len() + 1,
            liberty().layers.len(),
            "exactly one `building-3d` layer should have been removed"
        );
        assert_eq!(
            colour_digest(&liberty_without_the_new_layer),
            LIBERTY_COLOURS_AT_4A2101C,
            "a PRE-EXISTING colour in examples/liberty-style.json moved since 4a2101c -- it has \
             no \"lights\", so the lighting model must not have touched it. Do not re-pin this; \
             find the colour."
        );
        assert_eq!(
            colour_digest(&liberty()),
            LIBERTY_COLOURS_WITH_FLAT_BUILDING_3D,
            "examples/liberty-style.json's colours moved, and NOT because a layer was removed \
             from this assertion's input -- so `building-3d`'s own flat-footprint colour changed, \
             or a colour beside it did."
        );
        // Mapbox Streets gains real layers under THIS change too, so it now
        // needs the same split Liberty's `building-3d` needed: `road-label`
        // (2 `StyleLayer`s -- its `text-halo-color` is a 2-case `match`),
        // `path-pedestrian-label` (1) and `settlement-subdivision-label` (1)
        // all newly compile once `LayoutValue::ZoomOfMatchStep` exists (see
        // `compile_match_step_stops`), because their `text-size` was their
        // only remaining blocker (see
        // `compiles_most_of_the_real_vendored_mapbox_streets_style`'s entry
        // 20). Confirmed mechanically, the same way Liberty's split was:
        // with those 4 `StyleLayer`s removed, `colour_digest` comes back
        // 16837440352253820035 -- bit for bit the value pinned here since
        // 4a2101c -- so not one previously-compiling Mapbox Streets colour
        // moved.
        //
        // And again for the dropped-label-layers change: the twelve
        // `StyleLayer`s of `natural-line-label`, `water-line-label`,
        // `water-point-label`, `transit-label` and `airport-label` (see
        // `compiles_most_of_the_real_vendored_mapbox_streets_style`'s entry
        // 21) are new, and with them removed the digest is still the 4a2101c
        // value.
        let mut mapbox_streets_without_the_new_layers = or_skip!(mapbox_streets());
        mapbox_streets_without_the_new_layers.layers.retain(|layer| {
            !matches!(layer, StyleLayer::Symbol { id, .. }
                if ["road-label", "path-pedestrian-label", "settlement-subdivision-label", "natural-line-label",
                    "water-line-label", "water-point-label", "transit-label", "airport-label"]
                    .contains(&id.as_str()))
        });
        assert_eq!(
            mapbox_streets_without_the_new_layers.layers.len() + 4 + 12,
            or_skip!(mapbox_streets()).layers.len(),
            "exactly sixteen new StyleLayers (road-label x2, path-pedestrian-label, \
             settlement-subdivision-label, and the twelve of the dropped-label-layers change) \
             should have been removed"
        );
        assert_eq!(
            colour_digest(&mapbox_streets_without_the_new_layers),
            MAPBOX_STREETS_COLOURS_AT_4A2101C,
            "a PRE-EXISTING colour in examples/mapbox-streets-style.json moved since 4a2101c -- it \
             has no \"lights\", so the lighting model must not have touched it. Do not re-pin this; \
             find the colour."
        );
        assert_eq!(
            colour_digest(&or_skip!(mapbox_streets())),
            MAPBOX_STREETS_COLOURS_WITH_DROPPED_LABELS,
            "examples/mapbox-streets-style.json's colours moved, and NOT because a layer was \
             removed from this assertion's input -- so one of the new labels' own colour \
             changed, or a colour beside them did."
        );
    }
    /// Pins how `parse_and_compile` compiles a style with no `imports` key
    /// -- every vendored style so far, and the guarantee the whole Mapbox
    /// Standard series rests on: importing/config/light support must add
    /// new behaviour without moving this one. If this fails after a
    /// deliberate change, regenerate the two `{:#?}` dumps (e.g.
    /// temporarily add an `eprintln!("{:#?}", style)` where each fixture is
    /// compiled below, or restore a throwaway example like the one this
    /// test replaces), diff them against a checkout of the last commit
    /// where this test passed, confirm the diff is the change you meant to
    /// make, and update the constants below to the digests this test
    /// prints on failure.
    #[test]
    fn both_vendored_styles_compile_to_a_pinned_fingerprint() {
        // These were pinned at `4a2101c`, before the lighting-model branch.
        // They moved once, at `4021d15`, when `StyleLayer` gained an
        // `emissive_strength` field -- a `{:#?}` dump changes shape when a
        // field is added even though no colour moved -- and moved back when
        // that field was removed again, the value having been folded into
        // the colour instead of stored. Whether a COLOUR moved is not this
        // test's question and never was; see
        // `no_lights_no_lighting_every_vendored_colour_is_unchanged_since_before_the_branch`,
        // which answers it directly, must not be re-pinned, and is UNMOVED
        // by the change below.
        //
        // They moved a second time when `StyleLayer::Symbol` gained
        // `icon_opacity` and `text_opacity` -- the same field-addition
        // reason, and confirmed to be the ONLY reason rather than assumed
        // to be. The confirmation was mechanical: the `{:#?}` dump was taken
        // with those two fields' blocks deleted line-for-line and re-hashed
        // through this very function, and it came back
        //   liberty 13846795545035266429, streets 1685243431471727431
        // -- byte for byte the pair pinned before. Nothing else in either
        // compiled style moved: not a layer count, not a filter, not an
        // order, not a colour.
        //
        // And a third time, for `text_anchor` becoming a `TextAnchorValue`
        // and `text_offset_x`/`_y` learning `LayoutValue::ZoomOfDataRamp` --
        // the change that got POI labels off their own icons. Confirmed the
        // same way, and this one had a REAL behaviour change to isolate
        // rather than only a new field: the dump was rewritten back to what
        // the previous compiler produced for exactly those three fields
        // (`Constant(X)` unwrapped to the bare keyword, a `ZoomOfDataStep`
        // anchor rewound to the `Center` it used to degrade to, and a
        // `ZoomOfDataRamp` offset to the `Constant(0.0)` it used to degrade
        // to) and re-hashed, giving
        //   liberty 6271560231397674485, streets 12496770763405221866
        // -- again exactly the pair pinned before. So the whole of the diff
        // is those three fields on symbol layers.
        // And a FOURTH time, for the fill-extrusion fallback -- the first
        // move that is not a field addition but a layer addition. Liberty's
        // `building-3d` (`type: "fill-extrusion"`, `minzoom 14`) used to be
        // skipped for its layer type and now compiles to a flat
        // `StyleLayer::Fill` of its footprint, taking Liberty from 107
        // compiled layers to 108. That is the entire diff, and unlike the
        // three moves above it was confirmed by SUBTRACTION rather than by
        // reconstructing a dump: `building-3d` was removed from
        // `style.layers` and the remainder re-hashed through
        // `style_fingerprint`, giving
        //   liberty 12872894970650230042
        // -- byte for byte the number pinned here before. So every one of
        // the 107 layers that compiled before compiles to the identical
        // bytes: not a colour, not an opacity, not a filter, not a zoom
        // bound, not an order. `examples/mapbox-streets-style.json`
        // contains no `fill-extrusion` layer, and its fingerprint below is
        // therefore UNCHANGED -- the same number as before, which is itself
        // a second, independent check that the shared
        // `"fill" | "fill-extrusion"` arm did not disturb plain fills.
        //
        // And a FIFTH time, for `["geometry-type"]` becoming a real
        // predicate (`Filter::GeometryType`) instead of compiling to
        // `Filter::Any` and being dropped. Both fingerprints move, because
        // both styles use the expression -- Liberty 20 times, Streets 84.
        // Confirmed by comparing the full `{:#?}` dumps of both styles
        // either side of the change, with the `ZoomValue` interpolation
        // function pointers normalized out (they differ between builds):
        //
        //   * Layer counts are IDENTICAL -- Liberty 108, Streets 203 -- so
        //     no layer was added, lost, or emptied at compile time. The
        //     other three vendored styles were dumped the same way and are
        //     likewise unmoved: swat-biz-light 181, swat-routes-dark 104,
        //     mapbox-standard-dark 606.
        //   * Every REMOVED line in the diff is either an `Any,` placeholder
        //     (the dropped no-op) or part of the one filter that had to
        //     re-nest: `aeroway-line`'s `Not(In { key: "type", ... })`
        //     becomes `All([GeometryType([LineString]), Not(In { ... })])`
        //     because the geometry clause is no longer stripped from its
        //     `"all"` before compilation. Nothing else is removed: not a
        //     colour, not a width, not a zoom bound, not an order.
        //
        // What that costs at RENDER time was measured separately, against
        // real tiles rather than the compiled style: on the OpenFreeMap
        // Singapore z14 fixture, Liberty's matched-feature count over the
        // whole tile goes 10264 -> 10212, and every one of the 52 features
        // it now excludes is one a geometry predicate should exclude --
        // `water_name_line_label` no longer draws the 10 POINT water names
        // that `water_name_point_label` also drew (they were being labelled
        // twice), `highway-shield-non-us` no longer consumes the point
        // features of `transportation_name`, and Streets' `aeroway-line` no
        // longer strokes the outline of the tile's one aeroway POLYGON.
        // And a SIXTH time, for `fill-outline-color` becoming a real
        // compiled property (`StyleLayer::Fill::outline_color`) instead of
        // being read by nothing. BOTH fingerprints move, because both
        // styles declare the property on exactly two fill layers:
        //
        //   * Liberty -- `park` (a constant `rgba(95, 208, 100, 1)`) and
        //     `building` (an `interpolate` from `hsla(35,6%,79%,0.32)` at
        //     z13 to an opaque `hsl(35,6%,79%)` at z14);
        //   * Mapbox Streets -- `building` (`hsl(20, 10%, 79%)`) and
        //     `road-polygon` (`hsl(220, 20%, 85%)`), both constants.
        //
        // Neither layer COUNT moves -- Liberty 108, Streets 203 -- because
        // the outline joins the existing colour x opacity x emissive
        // cross-product as a fourth dimension, and each of those four
        // declarations folds to a single entry, so no layer is duplicated.
        //
        // The `fill-antialias` layers move nothing at all in either style:
        // Liberty's four are three `false` (none of which declares an
        // outline colour there would be anything to suppress) plus one
        // `true` on `landcover_wetland`, a pattern-only fill already
        // skipped whole; Streets' four (`landcover`, `landuse`,
        // `water-depth`, `hillshade`) are all `false` and likewise declare
        // no outline colour.
        // And a SEVENTH time, for the three `line-*` properties that shape
        // a stroke rather than colour it: `line-gap-width`,
        // `layout.line-cap` and `layout.line-join`. All three now compile
        // into `StyleLayer::Line`, so both fingerprints move even where no
        // layer count does -- and neither count DOES move here (Liberty
        // 108, Streets 203), for the same reason `fill-outline-color`
        // moved none: every declaration in both documents folds to a single
        // cross-product entry. Liberty declares `line-gap-width` once
        // (`waterway_tunnel`, a bare `2`), `line-cap` on 15 layers and
        // `line-join` on 44, all of them the literal `"round"`; Streets
        // declares the gap on 23 `*-case` layers as zoom-only
        // `interpolate`s, and writes its caps and joins as
        // `["step", ["zoom"], ...]` expressions that degrade to the spec
        // default (see `style::LineCap`).
        // And an EIGHTH time, for the `raster` layer type. Liberty's
        // `natural_earth` used to be skipped for its type and now compiles
        // to a `StyleLayer::Raster`, taking Liberty from 108 compiled
        // layers to 109. Like the fill-extrusion move, this is a layer
        // addition rather than a field addition -- and unlike every move
        // above, the confirmation by SUBTRACTION is not a comment
        // recording a check somebody once ran by hand. It is
        // `LIBERTY_FINGERPRINT_WITHOUT_RASTER` below, asserted on every
        // run: strip the one raster layer back out and the remainder must
        // hash to the number that was pinned here BEFORE this change.
        //
        // That is a much stronger claim than a re-pin, and it is the one
        // worth making: it says every one of the 108 layers that compiled
        // before compiles to identical bytes -- not a colour, not an
        // opacity, not a filter, not a zoom bound, not an order -- and it
        // keeps saying so. A future change that re-pinned the top number
        // while quietly moving a fill would fail on the bottom one.
        //
        // `examples/mapbox-streets-style.json` declares no raster layer,
        // so its fingerprint is UNCHANGED, which is a second and
        // independent check that the new early return in `compile_layer`
        // did not disturb the layer types that were already compiling.
        //
        // And a NINTH time, for string-keyed data-driven values
        // (`LayoutValue::ZoomOfMatchStep`, this change) -- another layer
        // addition, this time only on Mapbox Streets: `road-label`,
        // `path-pedestrian-label` and `settlement-subdivision-label` were
        // the three real layers blocked purely on a property-varying
        // `interpolate` whose stops are `match`es on a STRING tag (see
        // `compile_match_step_stops`'s doc comment), and all three now
        // compile, taking Streets from 203 compiled layers to 207 (see
        // `compiles_most_of_the_real_vendored_mapbox_streets_style`'s entry
        // 20). Confirmed by SUBTRACTION exactly like the raster move:
        // `MAPBOX_STREETS_FINGERPRINT_WITH_MATCH_SHAPED_LABELS` below is
        // asserted on every run, and the subtraction check right above it
        // strips those three layers back out and re-hashes, giving back
        // the number pinned here before this change. Liberty declares
        // none of these three shapes, so its fingerprint is UNCHANGED --
        // a second, independent check that the new
        // `compile_interpolated_data_step_layout_value` dispatch did not
        // disturb the `step`-shaped stops it already handled.
        const LIBERTY_FINGERPRINT: u64 = 1_835_538_834_098_286_374;
        /// Liberty with its one `raster` layer removed: the number
        /// `LIBERTY_FINGERPRINT` held before raster compiled.
        const LIBERTY_FINGERPRINT_WITHOUT_RASTER: u64 = 7_030_764_219_655_744_996;
        const MAPBOX_STREETS_FINGERPRINT: u64 = 474_843_290_366_046_145;
        /// Mapbox Streets with `road-label`, `path-pedestrian-label` and
        /// `settlement-subdivision-label` added: the fingerprint AFTER
        /// `LayoutValue::ZoomOfMatchStep` lets those three compile. See the
        /// subtraction check below for the mechanical confirmation that
        /// nothing else moved.
        const MAPBOX_STREETS_FINGERPRINT_WITH_MATCH_SHAPED_LABELS: u64 = 9_704_433_067_472_224_756;
        /// ...and with `settlement-major-label`/`-minor-label` (and
        /// `country-label`, a no-op under `center`) reading
        /// their `text-anchor` and `text-radial-offset` (the town dot fix).
        const MAPBOX_STREETS_FINGERPRINT_WITH_SETTLEMENT_ANCHORS: u64 = 831_072_335_738_717_938;
        /// ...and with the five label layers that used to be dropped
        /// compiling, plus `country-label`'s real `icon-opacity` (the
        /// eleventh subtraction check below).
        const MAPBOX_STREETS_FINGERPRINT_WITH_DROPPED_LABELS: u64 = 9_622_387_269_294_083_070;
        /// ...and with `road-intersection`'s `icon-text-fit` compiled (the
        /// fourteenth subtraction check below: reset it, and every number
        /// above still holds).
        const MAPBOX_STREETS_FINGERPRINT_WITH_ICON_TEXT_FIT: u64 = 14_364_127_269_888_441_085;

        // And a TWELFTH time (3D building extrusion, Task 4): Liberty's
        // `building-3d` compiles to a `StyleLayer::Extrusion` instead of a
        // flat `Fill`. Confirmed by SUBTRACTION: put back the `Fill` it used
        // to compile to -- same id, source layer, filter, colour, opacity
        // and zoom range, no outline -- and Liberty hashes to
        // `LIBERTY_FINGERPRINT` again. Everything below runs on that
        // pre-change style, so the older checks stay exactly as they were.
        const LIBERTY_FINGERPRINT_WITH_EXTRUSION: u64 = 14_335_734_914_750_342_146;
        // And a THIRTEENTH (Task 7 fix round 1, the Ruling amending R19): the
        // vendored `building-3d` gains `["!=", ["get", "hide_3d"], true]`.
        // Confirmed by SUBTRACTION: reset that one layer's filter to the
        // `Filter::Any` it compiled to without one, and Liberty hashes to
        // `LIBERTY_FINGERPRINT_WITH_EXTRUSION` again; every older check runs
        // on that style.
        const LIBERTY_FINGERPRINT_WITH_HIDE_3D_FILTER: u64 = 4_355_144_633_548_377_654;
        let liberty_with_hide_3d_filter = style_fingerprint(&liberty());
        let liberty = || {
            let mut style = liberty();
            let mut reset = 0;
            for layer in &mut style.layers {
                if let StyleLayer::Extrusion { filter, .. } = layer {
                    assert_eq!(
                        format!("{filter:?}"),
                        format!("{:?}", Filter::Not(Box::new(Filter::BoolEquals { key: "hide_3d".to_string(), value: true }))),
                        "building-3d's filter is exactly the hide_3d clause"
                    );
                    *filter = Filter::Any;
                    reset += 1;
                }
            }
            assert_eq!(reset, 1, "exactly building-3d");
            style
        };
        assert_eq!(
            liberty_with_hide_3d_filter, LIBERTY_FINGERPRINT_WITH_HIDE_3D_FILTER,
            "examples/liberty-style.json now compiles to a different Style \
             (actual fingerprint {liberty_with_hide_3d_filter}) -- see this test's doc comment"
        );
        let liberty_with_flat_building_3d = || {
            let mut style = liberty();
            let mut replaced = 0;
            for layer in &mut style.layers {
                if let StyleLayer::Extrusion { id, source_layer, filter, color, opacity, min_zoom, max_zoom, .. } = layer {
                    *layer = StyleLayer::Fill {
                        id: id.clone(),
                        source_layer: source_layer.clone(),
                        filter: filter.clone(),
                        color: color.clone(),
                        opacity: opacity.clone(),
                        outline_color: None,
                        min_zoom: *min_zoom,
                        max_zoom: *max_zoom,
                    };
                    replaced += 1;
                }
            }
            assert_eq!(replaced, 1, "exactly building-3d");
            style
        };

        let mut without_raster = liberty_with_flat_building_3d();
        let before = without_raster.layers.len();
        without_raster.layers.retain(|l| !matches!(l, StyleLayer::Raster { .. }));
        assert_eq!(
            before - without_raster.layers.len(),
            1,
            "this subtraction check is only meaningful if it removes exactly the one \
             raster layer Liberty declares"
        );
        let subtracted = style_fingerprint(&without_raster);
        assert_eq!(
            subtracted, LIBERTY_FINGERPRINT_WITHOUT_RASTER,
            "Liberty MINUS its raster layer now hashes to {subtracted}, not the number it \
             hashed to before raster compiled. So the raster layer is not the only thing \
             that changed -- something in the other 108 layers moved, and re-pinning the \
             fingerprint below would bury it. Find it."
        );

        let liberty_actual = style_fingerprint(&liberty_with_flat_building_3d());
        let liberty_with_extrusion = style_fingerprint(&liberty());
        assert_eq!(
            liberty_with_extrusion, LIBERTY_FINGERPRINT_WITH_EXTRUSION,
            "examples/liberty-style.json now compiles to a different Style \
             (actual fingerprint {liberty_with_extrusion}) -- see this test's doc comment"
        );
        assert_eq!(
            liberty_actual, LIBERTY_FINGERPRINT,
            "examples/liberty-style.json now compiles to a different Style \
             (actual fingerprint {liberty_actual}) -- see this test's doc comment"
        );

        // And a TENTH time: the settlement labels' `text-anchor` (a zoom
        // step over `["get", "text_anchor"]`) and `text-radial-offset` now
        // compile instead of degrading to `center` and nothing. Confirmed
        // by SUBTRACTION: put those two layers' two fields back to the
        // degraded values and the style must hash to the number pinned
        // before this change.
        // And an ELEVENTH: the dropped-label-layers change adds twelve
        // `StyleLayer`s (`natural-line-label`, `water-line-label` x2,
        // `water-point-label` x2, `transit-label` x6, `airport-label`; see
        // `compiles_most_of_the_real_vendored_mapbox_streets_style`'s entry
        // 21) and makes ONE existing field real: `country-label`'s
        // `icon-opacity`, `["step", ["zoom"], ["case", ["has",
        // "text_anchor"], 1, 0], 7, 0]`, which used to degrade to opaque.
        // Confirmed by SUBTRACTION: strip the twelve and put that one field
        // back to `opaque()`, and the style hashes to the number pinned
        // before this change. Everything below then runs on that
        // pre-change style, so the older checks stay exactly as they were.
        // And a FOURTEENTH: `icon-text-fit` compiles. Its default is cut out
        // of the dump (`style_fingerprint`); the one layer that declares it,
        // `road-intersection` (`both`, padding `[1, 2, 1, 2]`), is reset to
        // the default here by SUBTRACTION, and every older check below runs
        // on that pre-change style.
        let mut streets_before_dropped_labels = or_skip!(mapbox_streets());
        let mut fits_reset = Vec::new();
        for layer in &mut streets_before_dropped_labels.layers {
            if let StyleLayer::Symbol { id, icon_text_fit, .. } = layer {
                if *icon_text_fit != crate::style::IconTextFit::default() {
                    assert_eq!(
                        *icon_text_fit,
                        crate::style::IconTextFit { fit: crate::overlay::paint::TextFit::Both, padding: [1.0, 2.0, 1.0, 2.0] },
                        "{id}"
                    );
                    *icon_text_fit = crate::style::IconTextFit::default();
                    fits_reset.push(id.clone());
                }
            }
        }
        assert_eq!(fits_reset, ["road-intersection"], "exactly road-intersection declares icon-text-fit");
        let streets_before_dropped_labels_with_dropped = streets_before_dropped_labels.clone();
        let with_new = streets_before_dropped_labels.layers.len();
        streets_before_dropped_labels.layers.retain(|l| {
            !matches!(l, StyleLayer::Symbol { id, .. }
                if ["natural-line-label", "water-line-label", "water-point-label", "transit-label", "airport-label"]
                    .contains(&id.as_str()))
        });
        assert_eq!(with_new - streets_before_dropped_labels.layers.len(), 12, "the twelve new StyleLayers");
        let mut opacity_reverted = 0;
        for layer in &mut streets_before_dropped_labels.layers {
            if let StyleLayer::Symbol { id, icon_opacity, .. } = layer {
                if id == "country-label" {
                    *icon_opacity = crate::style::SymbolOpacity::opaque();
                    opacity_reverted += 1;
                }
            }
        }
        assert_eq!(opacity_reverted, 1, "exactly country-label");
        let streets_pre_change = style_fingerprint(&streets_before_dropped_labels);
        assert_eq!(
            streets_pre_change, MAPBOX_STREETS_FINGERPRINT_WITH_SETTLEMENT_ANCHORS,
            "Mapbox Streets minus the dropped-label-layers change's twelve layers and \
             country-label's icon-opacity hashes to {streets_pre_change}, not the number pinned \
             before that change -- something else moved. Find it."
        );

        let mut streets_degraded_anchor = streets_before_dropped_labels.clone();
        let mut reverted = 0;
        for layer in &mut streets_degraded_anchor.layers {
            if let StyleLayer::Symbol { id, text_anchor, text_radial_offset, .. } = layer {
                if id == "settlement-major-label" || id == "settlement-minor-label" {
                    *text_anchor = crate::style::TextAnchorValue::default();
                    *text_radial_offset = None;
                    reverted += 1;
                } else if id == "country-label" {
                    // `["step", ["zoom"], 0.6, 8, 0]` under the default
                    // `center` anchor: read now, and a no-op, because a
                    // radial offset from a centred anchor is `(0, 0)`.
                    assert!(text_radial_offset.is_some());
                    *text_radial_offset = None;
                    reverted += 1;
                }
            }
        }
        assert_eq!(reverted, 3, "exactly the two settlement label layers and country-label");
        let streets_reverted = style_fingerprint(&streets_degraded_anchor);
        assert_eq!(
            streets_reverted, MAPBOX_STREETS_FINGERPRINT_WITH_MATCH_SHAPED_LABELS,
            "Mapbox Streets with its settlement labels' anchor and radial offset put back to \
             the degraded `center`/none hashes to {streets_reverted}, not the number pinned \
             before they compiled -- something else moved. Find it."
        );

        let mut streets_without_the_new_labels = streets_degraded_anchor.clone();
        let streets_before = streets_without_the_new_labels.layers.len();
        streets_without_the_new_labels.layers.retain(|l| {
            !matches!(l, StyleLayer::Symbol { id, .. }
                if id == "road-label" || id == "path-pedestrian-label" || id == "settlement-subdivision-label")
        });
        assert_eq!(
            streets_before - streets_without_the_new_labels.layers.len(),
            4,
            "this subtraction check is only meaningful if it removes exactly the four new \
             StyleLayers (road-label x2, path-pedestrian-label, settlement-subdivision-label)"
        );
        let streets_subtracted = style_fingerprint(&streets_without_the_new_labels);
        assert_eq!(
            streets_subtracted, MAPBOX_STREETS_FINGERPRINT,
            "Mapbox Streets MINUS its three newly-string-match-shaped labels now hashes to \
             {streets_subtracted}, not the number it hashed to before `LayoutValue::ZoomOfMatchStep` \
             existed. So those three layers are not the only thing that changed -- something in the \
             other 203 layers moved, and re-pinning the fingerprint below would bury it. Find it."
        );

        let streets_actual = style_fingerprint(&or_skip!(mapbox_streets()));
        assert_eq!(
            style_fingerprint(&streets_before_dropped_labels_with_dropped), MAPBOX_STREETS_FINGERPRINT_WITH_DROPPED_LABELS,
            "Mapbox Streets with road-intersection's icon-text-fit reset no longer hashes to the \
             number pinned before icon-text-fit compiled -- something else moved"
        );
        assert_eq!(
            streets_actual, MAPBOX_STREETS_FINGERPRINT_WITH_ICON_TEXT_FIT,
            "examples/mapbox-streets-style.json now compiles to a different Style \
             (actual fingerprint {streets_actual}) -- see this test's doc comment"
        );
    }

    #[test]
    fn carries_each_vendored_styles_own_sprite_reference_through_verbatim() {
        // Unresolved on purpose -- resolving `mapbox://` needs an access
        // token, which is `sprite::resolve_sprite_url`'s job, not this
        // parser's. What matters here is that the field survives at all:
        // without it every example had to re-read the style JSON to find
        // out where its icons live, which is exactly why the Liberty demo
        // never fetched a sheet.
        assert_eq!(
            liberty().sprite.as_deref(),
            Some("https://tiles.openfreemap.org/sprites/ofm_f384/ofm")
        );
        assert_eq!(or_skip!(mapbox_streets()).sprite.as_deref(), Some("mapbox://sprites/mapbox/streets-v12"));
    }

    #[test]
    fn a_style_with_no_sprite_field_compiles_with_none() {
        // Every hand-written style in this file's tests is one, and a
        // missing sheet is "no icons", never a compile failure.
        let style = parse_and_compile(br#"{"version": 8, "layers": []}"#).unwrap();
        assert_eq!(style.sprite, None);
    }

    /// The acceptance case for import composition: the whole vendored
    /// Mapbox Standard Dark document, compiled end to end, with every
    /// remaining skip named and accounted for.
    ///
    /// Measured, not guessed. The imported document declares 150 layers:
    /// 12 are types this compiler has never drawn (`model` 5, `circle` 2,
    /// `building` 1, `clip` 1, plus the 3 `slot` markers), 39 skip for
    /// constructs listed below, and the remaining
    /// **99 compile into 557 `StyleLayer`s** -- the basemap's background,
    /// 14 of its 18 fills, every one of its 74 lines, and its 10
    /// `["image", name, {"params": ...}]` symbol layers (the nine one-way
    /// arrows and `crosswalks`, one `StyleLayer` apiece). 557, not the 274
    /// a plain colour/width/opacity/dash cross-product alone would give:
    /// `roads`/`bridge-roads` each expand into 2 `StyleLayer::Line`s per
    /// pre-existing colour/width case (78 apiece, not 39) once
    /// `line-emissive-strength` is cross-producted in too, since their real
    /// expression genuinely varies by `class` (see
    /// `compile_emissive_strength`'s and `compile_let`'s doc comments); and
    /// `landuse` alone contributes 195, its 13-case `fill-color` `match`
    /// crossed with its equally class-driven `fill-opacity`.
    ///
    /// What this test is really guarding is the negative: **not one fill or
    /// line is skipped, or degraded, because of `config` or `measure-light`
    /// folding.** The 4 fill skips are each a different, already-known
    /// construct, and every one of the 74 line layers compiles. Before this
    /// series none of these layers compiled at all, because
    /// `parse_and_compile` never looked inside `imports`.
    ///
    /// The fill-extrusion fallback moved 6 layers from the first bucket
    /// (unsupported type, 18 -> 12) into the second (skipped, 33 -> 39) and
    /// moved NOTHING else: `compiled` is still 99, `style_layers` still
    /// 557, and the end-to-end count still 604. Each of the 6 is skipped
    /// for a construct that predates this change and is not specific to
    /// extrusion at all -- see the assertion on `skipped_extrusions` below,
    /// which names them and says why. Standard's buildings therefore still
    /// do not draw; Liberty's now do.
    /// The structured diagnostics are the same facts the stderr notices
    /// carry, in a form something other than a human reading a terminal
    /// can act on.
    ///
    /// Before this, the ONLY place `parse_and_compile` said why a layer was
    /// missing was an `eprintln!` -- which is why
    /// `examples/mapbox_standard_dark.rs` captures the process's own
    /// stderr and parses it back. `map.get_diagnostics` cannot do that, and
    /// an agent asking "why is nothing drawing?" needs the answer as data.
    #[test]
    fn a_compile_records_its_skips_hides_and_degradations_as_structured_diagnostics() {
        let document = serde_json::json!({
            "version": 8,
            "layers": [
                { "id": "kept", "type": "background", "paint": { "background-color": "#123456" } },
                // A layer type this renderer has no equivalent for.
                { "id": "dropped-for-type", "type": "heatmap", "source-layer": "x" },
                // Switched off by its author.
                {
                    "id": "switched-off",
                    "type": "fill",
                    "source-layer": "water",
                    "layout": { "visibility": "none" },
                    "paint": { "fill-color": "#000000" }
                },
            ],
        });
        let style = parse_and_compile(document.to_string().as_bytes()).expect("this compiles");
        assert_eq!(style.layers.len(), 1, "only the background layer draws");

        // THIS thread's compile, not the process-global gauge: other tests
        // in this binary compile styles on their own threads.
        let diagnostics = last_compile_diagnostics();

        let skipped: Vec<&StyleDiagnostic> =
            diagnostics.iter().filter(|d| d.kind == DiagnosticKind::SkippedLayer).collect();
        assert_eq!(skipped.len(), 1, "expected exactly one skip, got {diagnostics:#?}");
        assert_eq!(skipped[0].layer.as_deref(), Some("dropped-for-type"));
        assert!(
            skipped[0].detail.contains("unsupported layer type"),
            "the detail must say WHY: {:?}",
            skipped[0].detail
        );

        // A `visibility: "none"` written as a bare string is the plain
        // case, decided without folding anything -- deliberately NOT
        // recorded, because "the author switched this off" is not a
        // diagnostic about this crate. Only a visibility EXPRESSION that
        // would not fold is.
        assert!(
            diagnostics.iter().all(|d| d.layer.as_deref() != Some("switched-off")),
            "an explicitly hidden layer is not a degradation: {diagnostics:#?}"
        );
    }

    /// A visibility expression the folder cannot reduce hides the layer --
    /// loudly, and now structurally, because "your map is missing a whole
    /// category and here is the config key it hangs off" is the single
    /// most useful thing this can tell a caller.
    #[test]
    fn an_unfoldable_visibility_expression_is_recorded_as_a_hidden_layer() {
        let document = serde_json::json!({
            "version": 8,
            "layers": [{
                "id": "config-gated",
                "type": "fill",
                "source-layer": "water",
                "layout": { "visibility": ["config", "showWaterUnknownKey"] },
                "paint": { "fill-color": "#000000" }
            }],
        });
        let style = parse_and_compile(document.to_string().as_bytes()).expect("this compiles");
        assert!(style.layers.is_empty(), "an unfoldable visibility hides the layer");

        let hidden: Vec<StyleDiagnostic> = last_compile_diagnostics()
            .into_iter()
            .filter(|d| d.kind == DiagnosticKind::HiddenLayer)
            .collect();
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].layer.as_deref(), Some("config-gated"));
    }

    /// The global gauge is what `map.get_diagnostics` reads, so it must
    /// actually be published -- a thread-local that never reached it would
    /// pass every test above and report nothing over the wire.
    #[test]
    fn the_published_gauge_holds_a_whole_compile_not_a_fragment() {
        let document = serde_json::json!({
            "version": 8,
            "layers": [{ "id": "only-layer", "type": "heatmap", "source-layer": "x" }],
        });
        parse_and_compile(document.to_string().as_bytes()).expect("this compiles");
        // Not asserted to be OUR compile: another test's compile can win
        // the race for the global between these two lines, and that is
        // documented behaviour. What is asserted is the invariant that
        // holds either way -- the published set is a whole compile's worth,
        // never a half-written one, so it can never exceed the cap or
        // contain a diagnostic with an empty detail.
        for diagnostic in compile_diagnostics() {
            assert!(!diagnostic.detail.is_empty(), "a published diagnostic must say something");
        }
        assert!(compile_diagnostics().len() <= MAX_DIAGNOSTICS);
    }

    #[test]
    fn the_vendored_mapbox_standard_dark_fixture_compiles_with_only_accounted_for_skips() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = parse_imports(&root).unwrap();
        let import = &imports[0];
        let scope = Scope::root().with_config(&import.config).with_lights(&import.data["lights"]);
        let declared = import.data["layers"].as_array().unwrap();
        assert_eq!(declared.len(), 150, "fixture shape moved out from under this test");

        let mut unsupported_type: Vec<&str> = Vec::new();
        let mut skipped: Vec<(&str, &str)> = Vec::new();
        let mut compiled = 0usize;
        let mut style_layers = 0usize;
        for layer in declared {
            let id = layer["id"].as_str().unwrap();
            let layer_type = layer["type"].as_str().unwrap();
            assert_ne!(
                layer.get("layout").and_then(|l| l.get("visibility")).and_then(Value::as_str),
                Some("none"),
                "the imported document hides no layers; {id} now does, so these counts need revisiting"
            );
            // Mirrors `compile_single_layer`'s own type gate, and must keep
            // mirroring it: `fill-extrusion` moved from this list into the
            // compiler when the flat-footprint fallback landed, and leaving
            // the mirror stale would have quietly stopped this test putting
            // those 6 layers through `compile_layer` at all. `raster`
            // joined the gate with the raster layer type; this fixture
            // declares none, so the counts below are unmoved by it -- and
            // that is a fact worth having the mirror state rather than a
            // coincidence worth relying on.
            if !matches!(layer_type, "background" | "fill" | "line" | "symbol" | "fill-extrusion" | "raster") {
                unsupported_type.push(id);
                continue;
            }
            let out = compile_layer(id, layer_type, layer, scope).expect("no imported layer is structurally broken");
            if out.is_empty() {
                skipped.push((layer_type, id));
            } else {
                compiled += 1;
                style_layers += out.len();
            }
        }

        // 12, not the 18 pinned before the fill-extrusion fallback: the 6
        // `fill-extrusion` layers now go through `compile_layer` instead of
        // being turned away at the type gate. The remaining 12 are `model`
        // 5, `circle` 2, `building` 1, `clip` 1, and the 3 `slot` markers.
        assert_eq!(unsupported_type.len(), 12, "unsupported layer types: {unsupported_type:?}");

        // 4, not the 6 pinned when the fill-extrusion fallback landed.
        // That commit found all 6 still skipped, for two constructs with
        // nothing to do with extrusion, and pinned them here so the reasons
        // stayed visible. One of the two has now been answered:
        //
        // - `2d-building` and `3d-building` skipped on
        //   `fill-extrusion-color` being a `["case", ["to-boolean",
        //   ["feature-state", "select"]], ...]`. `["feature-state", k]` now
        //   folds to `null` -- what the reference returns for a map with no
        //   state set, which this crate can never have (see `eval_inner`'s
        //   `"feature-state"` arm) -- so `to-boolean` gives `false`, the
        //   `case` takes the else branch the author wrote for exactly this,
        //   and both layers compile. The SAME construct was skipping the
        //   plain `fill` layer `building-underground`, which now compiles
        //   too; it is gone from `skipped_fills` below.
        // - The 4 `indoor-*` layers still skip, on the `is-active-floor`
        //   filter operator, over an `indoor` source this crate does not
        //   load -- again the same construct that still skips the plain
        //   `fill` layers `indoor-floor` and `indoor-security-zone`. That
        //   one is untouched here and remains the reason to revisit.
        let skipped_extrusions: Vec<&str> =
            skipped.iter().filter(|&&(t, _)| t == "fill-extrusion").map(|&(_, id)| id).collect();
        assert_eq!(
            skipped_extrusions,
            vec!["indoor-building-walls", "indoor-doors", "indoor-rooms", "indoor-rooms-walls"],
            "the set of skipped fill-extrusion layers changed"
        );

        // THE assertion. Every line layer compiles; the only fills that do
        // not are these three, and not one of them is a colour that failed
        // to fold. Named individually so a regression says WHICH layer went
        // missing, not just that the count moved.
        assert!(
            !skipped.iter().any(|&(layer_type, _)| layer_type == "line"),
            "no line layer may be skipped -- these were: {:?}",
            skipped.iter().filter(|&&(t, _)| t == "line").collect::<Vec<_>>()
        );
        let skipped_fills: Vec<&str> = skipped.iter().filter(|&&(t, _)| t == "fill").map(|&(_, id)| id).collect();
        assert_eq!(
            skipped_fills,
            vec![
                // `landuse` USED to be first in this list: its filter is
                // `["case", ["==", ["config", "theme"], "monochrome"], ...]`
                // and `compile_filter` had neither a scope nor a folder, so
                // a config-driven filter operand was an unsupported shape.
                // It now folds (see `filter::compile_config_comparison`) and
                // the layer compiles -- the urban-fabric fill is no longer
                // missing from this document.
                // ["interpolate", ["linear"], ["get", "min_depth"], ...]:
                // feature-driven, exactly as in Mapbox Streets, where it is
                // correctly skipped today.
                "water-depth",
                // `building-underground` USED to be next in this list, on
                // `["to-boolean", ["feature-state", "select"]]` in its
                // `fill-color`. It compiles now, for the reason given above
                // `skipped_extrusions`: an unset feature-state is `null`,
                // so the `case` folds to the else branch and this fill --
                // which has nothing to do with 3D or extrusion -- draws.
                // Filter operator "is-active-floor", on the `indoor` source
                // this crate does not load; matches no features either way.
                "indoor-floor",
                "indoor-security-zone",
            ],
            "the set of skipped fills changed"
        );

        // Symbol layers are the rest, and none of their skips is a colour
        // fold either. Attributed by reading each skip's own message:
        // 18 `["pitch"]` filters (Phase 3, deferred by the design doc);
        // 5 the `indoor` source's `is-active-floor` filter operator; 3
        // `symbol-placement: "line-center"`, out of scope by design; and one
        // each for `coalesce` (`road-number-shield`), `in` (`indoor-label`)
        // and `number` (`settlement-subdivision-label`) as filter operands.
        // Not one is a `config` or `measure-light` failure.
        //
        // 29, not the 39 this test pinned before `icon-image` learned the
        // style spec's own `["image", name, options]` expression and the
        // `["let", ...]` wrapper the ten arrow/crosswalk layers write it
        // inside: those ten were the whole of the "10 `icon-image` shapes"
        // bucket that used to sit in this list, and they now compile. See
        // `note_dropped_image_options` for the one thing they give up.
        let skipped_symbols = skipped.iter().filter(|&&(t, _)| t == "symbol").count();
        assert_eq!(skipped_symbols, 29, "skipped symbol layers: {skipped:?}");

        // 102, not 99: `building-underground`, `2d-building` and
        // `3d-building`, all three of which used to skip on
        // `["to-boolean", ["feature-state", ...]]` and now fold it away.
        assert_eq!(compiled, 102, "imported layers that compiled");
        // 560, not 557: exactly one `StyleLayer` each for those same three
        // -- none of their paint values is feature- or zoom-branching once
        // the state gates fold, so none of them cross-products.
        //
        // 557, not 274: `roads` and `bridge-roads` -- both class-based,
        // `emissive-strength`-varying since `compile_let` started inlining
        // their real `isHighClass` binding (see that function's doc
        // comment) -- cross-product each of their 39 pre-existing
        // colour/width cases with 2 emissive-strength cases (motorway/
        // trunk vs. everything else), doubling 39 -> 78 apiece. The
        // remaining +195 is `landuse` alone, newly compiling now that its
        // config-driven filter folds: its `fill-color` is a 13-case `match`
        // on `class`, its `fill-opacity` a zoom `interpolate` whose stops
        // are `match`es on the same property (13 more cases once the
        // nested default `match` is flattened -- see `parse_match_segment`),
        // and the two cross-product. The final +10 is one
        // `StyleLayer::Symbol` each for the nine one-way-arrow layers and
        // `crosswalks`, newly compiling now that `["image", name,
        // {"params": ...}]` resolves to a sprite NAME.
        //
        // 920, not 560: `line-gap-width` is now compiled and cross-producted
        // with the other line paint properties (see `compile_layer`'s
        // `"line"` arm). Three of this document's 36 gap-width layers drive
        // theirs from a `["match", ["get", "class"], ...]` -- `roads-case`
        // (4 class groups), `tunnel-roads-case` (3) and `road-minor-case`
        // (1) -- and the first two are exactly the layers whose colour and
        // width are ALREADY 39-case `class` matches, so the gap's cases
        // multiply rather than add: +360 across the three. Most of those
        // pairings are contradictory (`class == motorway` from the width
        // ANDed with `class == street` from the gap) and match no feature,
        // which is the standing cost of this compiler's cross-product
        // strategy rather than anything new -- it is what `line-color` x
        // `line-width` x `line-emissive-strength` already did to the same
        // three layers. No style this renderer actually draws pays it: the
        // 23 `*-case` layers of `examples/mapbox-streets-style.json` and the
        // one in Liberty all ramp their gap with zoom alone, which expands
        // to a single case.
        assert_eq!(style_layers, 920, "StyleLayers the imported document compiles to");

        // And end to end, through the public entry point, with the root
        // document's own 88 layers (48 of them hidden) stacked on top.
        //
        // 560 + 48 - 2, not 560 + 48: `compile_layer` above is called
        // directly, bypassing the `layout.visibility` gate this test
        // deliberately doesn't exercise (see the `assert_ne!` above), so
        // `style_layers` above still counts every layer whose EXPRESSION-
        // valued `visibility` the real `parse_and_compile` folds and hides.
        // This document has 76 such layers (config keys
        // `showPedestrianRoads`, `show3dObjects`/`show3dBuildings`/
        // `show3dFacades`/`show3dTrees`/`show3dLandmarks`,
        // `showAdminBoundaries`, `showIndoor`(`Labels`), `showRoadLabels`,
        // `showPlaceLabels`, `showPointOfInterestLabels`,
        // `showTransitLabels`, `showLandmarkIcons`); of those, 8 are
        // already excluded above as `unsupported_type` (`model`/`circle`/
        // `building`/`clip`) and 35 more are already IN `skipped` for a
        // construct entirely unrelated to visibility (chiefly a
        // `["<=", ["pitch"], n]` layer filter this compiler doesn't
        // support -- among them `natural-point-label`, `water-point-
        // label`, `settlement-major-label`, `country-label`, the very
        // "place label" layers `showPlaceLabels` gates; they draw nothing
        // either way, visibility bug or no).
        //
        // 35, not the 37 pinned before `feature-state` folded: `2d-building`
        // and `3d-building` have left that bucket and joined the 33 that
        // DO compile (`building-underground`, the third layer to start
        // compiling, has no `visibility` at all and was never among the 76).
        //
        // Of those 33, this fixture's own config (`showIndoor: true`,
        // `show3dBuildings`/`show3dObjects: false`) hides exactly two:
        // `indoor-building-footprint` (`["case", ["all", ["config",
        // "showIndoor"], ["config", "show3dObjects"]], "visible", "none"]`)
        // and now `3d-building`, whose `visibility` is the same shape over
        // `show3dBuildings`/`show3dObjects` and folds to `"none"` -- two
        // fewer `StyleLayer`s end to end. `2d-building` writes that same
        // condition with its outputs the other way round (`"none"` first),
        // so it folds to `"visible"` and draws: under this config the
        // document deliberately shows the flat footprint INSTEAD of the
        // extruded one, which is exactly the layer this crate can honour.
        // The other 31 fold to `"visible"` and are unaffected.
        let style = parse_and_compile(&bytes).expect("the fixture must compile");
        // 48, not the 45 the root document used to contribute: its own
        // `tunnel-oneway-arrow-blue` overlay layer joins them, compiling now
        // that a zoom bracket's value may itself be a `["match", ["get",
        // key], ...]` (`DataIconShape::MatchGet`), and so does
        // `transit-stop-label` -- Singapore's bus stops and MRT stations --
        // now that a `text-offset` may be a zoom `interpolate` over
        // `["literal", [x, y]]` stops, and so does `poi-label`, whose `let`
        // filter now inlines (degrading to `Filter::Any`, see
        // `compile_let_filter`) and whose `["concat", ["get", "maki"],
        // "-11"]` icon name a zoom bracket can now hold.
        //
        // 49, not 48 (this change): the root document's own `"path
        // pedestrian label"` overlay -- a space-named layer distinct from
        // the import's hyphenated `path-pedestrian-label` above -- newly
        // compiles now that `LayoutValue::ZoomOfMatchStep` exists (see
        // `compile_match_step_stops`). Its `text-size` is exactly
        // `path-pedestrian-label`'s real shape (an `interpolate` whose two
        // zoom stops are both `["match", ["get", "class"], ["pedestrian"],
        // ...]`, same single label group at both stops), its filter is a
        // zoom `step` of `match`-as-boolean cases (already supported, no
        // `["pitch"]` clause unlike the import's version), and its
        // `text-color`/`text-halo-color` are both plain constants, so it
        // contributes exactly one `StyleLayer`. Confirmed to be the ONLY
        // layer this change moves in this fixture: the root's own `"road
        // label"` has the same match-shaped FIRST stop but a bare-number
        // SECOND stop (`18, 13`, no `match` at all) -- a mismatched-shape
        // stop list `compile_match_step_stops` refuses same as before -- and
        // the import's `road-label`/`path-pedestrian-label` both carry an
        // unrelated, pre-existing `["pitch"]` filter clause that keeps them
        // skipped regardless (see `skipped_symbols` above); the import's
        // `settlement-subdivision-label` mixes a `"case"` and a `"match"`
        // stop output (still refused) and ALSO carries a `["number", ...]`
        // filter clause (also pre-existing and unrelated).
        //
        // 56, not 49 (the dropped-label-layers change): seven root-document
        // labels newly compile, one `StyleLayer` each, measured by diffing
        // the compiled symbol ids against the previous commit's:
        // `waterway label` and `water line label` (`symbol-placement:
        // "line-center"`; the latter's `text-size` is a zoom `interpolate`
        // of property `interpolate`s, now a `LayoutValue::FeatureExpression`),
        // `airport-label` and `state-label` (a `text-field` `step` on a
        // property / on zoom over property `step`s), `country-label`,
        // `city label` and `settlement-minor-label` (their remaining
        // blocker was a `text-field` zoom `step`, or a property `step`
        // in a paint colour).
        assert_eq!(
            style.layers.len(),
            920 + 56 - 2,
            "hides indoor-building-footprint and 3d-building (show3dObjects: false)"
        );
    }

    /// The acceptance case for `["config", k]` as a filter operand, on the
    /// layer that motivated it. Mapbox Standard's `landuse` -- the urban-
    /// fabric fill, and the single largest hole this document had -- guards
    /// one clause of its filter with `["case", ["==", ["config", "theme"],
    /// "monochrome"], ...]`. Before `filter::compile_config_comparison`
    /// that shape was an unsupported operand and the whole layer was
    /// skipped.
    ///
    /// Compiling is not drawing (that distinction has cost this crate a
    /// review round before), so this asserts the second thing too: a real
    /// `landuse` feature's tags pass one of the compiled layers' filters,
    /// and that layer's `fill-opacity` at that zoom is not zero.
    #[test]
    fn the_config_driven_landuse_filter_compiles_and_draws() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let root: Value = serde_json::from_slice(&bytes).unwrap();
        let imports = parse_imports(&root).unwrap();
        let import = &imports[0];
        assert_eq!(
            import.config.resolve("theme"),
            Some(&serde_json::json!("monochrome")),
            "this fixture pins theme: monochrome; the assertions below are written for that branch"
        );
        let scope = Scope::root().with_config(&import.config).with_lights(&import.data["lights"]);
        let layer = import.data["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["id"] == serde_json::json!("landuse"))
            .expect("fixture shape moved out from under this test");

        let compiled = compile_layer("landuse", "fill", layer, scope).expect("structurally sound");
        assert!(!compiled.is_empty(), "landuse must no longer be skipped");

        // A real `landuse` polygon: a city park. `sizerank` satisfies the
        // filter's two arithmetic clauses, `type` is none of the four the
        // filter excludes, and `class` is neither `parking` nor `sand`, so
        // the config-guarded monochrome clause admits it too.
        let park = HashMap::from([
            ("class".to_string(), TagValue::String("park".to_string())),
            ("type".to_string(), TagValue::String("park".to_string())),
            ("sizerank".to_string(), TagValue::Number(1.0)),
        ]);
        let drawn: Vec<&StyleLayer> = compiled
            .iter()
            .filter(|l| match l {
                StyleLayer::Fill { filter, opacity, .. } => filter.matches(&park, 12.0) && opacity.value_at(12) > 0.0,
                _ => false,
            })
            .collect();
        assert_eq!(drawn.len(), 1, "exactly one of landuse's compiled cases must paint a park at z12");

        // And the same feature under a config whose theme is NOT
        // monochrome: the guarded clause takes its `true` fallback there,
        // so a park still draws -- the fold must not silently delete
        // features from the other branch.
        // Mutating the one key on the real document, rather than building a
        // config by hand: the layer's colours read five more config keys and
        // its lights read others still, so a hand-built config would test
        // the degradation path instead of the branch this is about.
        let mut faded_root = root.clone();
        faded_root["imports"][0]["config"]["theme"] = serde_json::json!("faded");
        let faded_imports = parse_imports(&faded_root).unwrap();
        let faded = &faded_imports[0];
        let faded_scope = Scope::root().with_config(&faded.config).with_lights(&faded.data["lights"]);
        let compiled = compile_layer("landuse", "fill", layer, faded_scope).expect("structurally sound");
        assert!(
            compiled.iter().any(|l| matches!(l, StyleLayer::Fill { filter, opacity, .. } if filter.matches(&park, 12.0) && opacity.value_at(12) > 0.0)),
            "a park must still draw when the config comparison folds the other way"
        );
    }

    /// The other half of the acceptance: a filter whose config comparison
    /// is FALSE must still COMPILE. Skipping the layer would be wrong --
    /// the document is well-formed, the author simply wrote a clause that
    /// this configuration does not satisfy -- and so would drawing it.
    #[test]
    fn a_layer_whose_config_comparison_is_false_compiles_but_matches_nothing() {
        let config = ResolvedConfig::from_pairs([("theme", serde_json::json!("faded"))]);
        let scope = Scope::root().with_config(&config);
        let layer: Value = serde_json::json!({
            "id": "monochrome-only",
            "type": "fill",
            "source-layer": "landuse",
            "filter": ["==", ["config", "theme"], "monochrome"],
            "paint": {"fill-color": "#ff0000"}
        });

        let compiled = compile_layer("monochrome-only", "fill", &layer, scope).expect("structurally sound");
        assert_eq!(compiled.len(), 1, "a false config comparison must not skip the layer");
        let StyleLayer::Fill { filter, .. } = &compiled[0] else { panic!("expected a fill: {:?}", compiled[0]) };
        assert!(!filter.matches(&HashMap::new(), 14.0), "a false config comparison must match no feature");
        assert!(
            !filter.matches(&HashMap::from([("class".to_string(), TagValue::String("park".to_string()))]), 14.0),
            "a false config comparison must match no feature, tagged or not"
        );

        // The same layer under the config the comparison is written for:
        // now it matches everything the source layer offers.
        let matching = ResolvedConfig::from_pairs([("theme", serde_json::json!("monochrome"))]);
        let compiled = compile_layer("monochrome-only", "fill", &layer, Scope::root().with_config(&matching)).unwrap();
        let StyleLayer::Fill { filter, .. } = &compiled[0] else { panic!("expected a fill") };
        assert!(filter.matches(&HashMap::new(), 14.0));
    }

    /// Constraint 2, pinned rather than eyeballed: **not one paint property
    /// in the whole document degrades to a literal.**
    ///
    /// This is the assertion the layer counts above cannot make. A property
    /// that degrades keeps its layer, so the count is unchanged and the only
    /// evidence is an `eprintln!` nobody reads -- which is how a real
    /// defect (`recreation-polygon` compiling to opaque black under a
    /// `dusk` config) came within one review of shipping.
    ///
    /// `roads`/`bridge-roads`' real `line-emissive-strength` used to be a
    /// SECOND, always-present exception here (a feature-dependent `let`
    /// binding, categorically refused): `compile_let` now inlines it
    /// instead (see that function's doc comment), so both fold cleanly --
    /// this fixture's zero-degradations count is back to true zero, plus
    /// only the one pre-existing `recreation-polygon` exception below.
    #[test]
    fn the_vendored_mapbox_standard_dark_fixture_compiles_with_no_folder_degradations() {

        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let _ = take_degradations();
        let style = parse_and_compile(&bytes).expect("the fixture must compile");
        assert!(!style.layers.is_empty());
        let (capability, degraded) = split_degradations(take_degradations());
        assert!(degraded.is_empty(), "the fixture must fold every paint property it reaches:\n{}", degraded.join("\n"));
        assert_capability_degradations(&capability);

        // And under every lightPreset the fixture's own schema offers, not
        // just the `night` the config pins -- `dawn` is the one whose
        // brightness lands strictly between two of `recreation-polygon`'s
        // stops, and it is invisible to the pinned-config run above.
        //
        // Mutating one config key at a time (rather than replacing the
        // whole 20-key `imports[0].config` object) keeps the other 19
        // overrides -- `colorWater` among them -- at the values the shipped
        // document actually sets, instead of falling back to schema
        // defaults nothing renders.
        //
        // Two things are pinned per combination, not one:
        //  - the layer count, so a compile that starts silently DROPPING
        //    layers (as it does today if `"config"` is pulled out of
        //    `FOLDED_OPERATORS`, or if `measure_light_brightness` is
        //    stubbed) fails here instead of passing 11 of 12 iterations
        //    vacuously green;
        //  - the exact set of degradation notices, not just emptiness --
        //    `dawn` (brightness 0.396140, per `calculateLightsBrightness`
        //    -- fix round 2 of the task-2 report; this exception used to be
        //    `dusk` under the earlier, wrong brightness approximation)
        //    lands strictly inside `recreation-polygon`'s fill-color domain
        //    under two of the three themes (`default` and `faded`;
        //    `monochrome`'s 0.414247 clamps just above it instead), firing
        //    the nearest-stop approximation once each. That is a real,
        //    known gap in this compile, and pinning it here is what keeps a
        //    THIRD combination starting to degrade from going unnoticed.
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        for preset in ["dawn", "day", "dusk", "night"] {
            for theme in ["default", "monochrome", "faded"] {
                root["imports"][0]["config"]["lightPreset"] = serde_json::json!(preset);
                root["imports"][0]["config"]["theme"] = serde_json::json!(theme);
                let document = serde_json::to_vec(&root).unwrap();
                let _ = take_degradations();
                let style = parse_and_compile(&document).expect("every preset must compile");
                assert_eq!(
                    style.layers.len(),
                    // - 2: `indoor-building-footprint` and `3d-building`.
                    // Both hide on a `layout.visibility` that folds against
                    // `showIndoor`/`show3dBuildings`/`show3dObjects`, not
                    // `lightPreset`/`theme` -- none of those varies across
                    // this loop's combinations, so both stay hidden (this
                    // fixture sets `show3dObjects: false`) the same way in
                    // all twelve. See
                    // `the_vendored_mapbox_standard_dark_fixture_compiles_with_only_accounted_for_skips`
                    // for the rest of the 76 expression-valued-visibility
                    // layers this document has and why only these two move
                    // the count. 49, not 48: see
                    // `the_vendored_mapbox_standard_dark_fixture_compiles_with_only_accounted_for_skips`'s
                    // own comment on the root `"path pedestrian label"`
                    // overlay, which now compiles in every preset/theme
                    // combination (its `text-size`, filter and paint are all
                    // constant across `lightPreset`/`theme`). 56, not 49:
                    // the seven root labels the dropped-label-layers change
                    // brings in (same test's comment).
                    920 + 56 - 2,
                    "lightPreset {preset:?} + theme {theme:?}: layer count changed -- a layer was silently dropped"
                );
                let (capability, degraded) = split_degradations(take_degradations());
                // None of these three gaps has anything to do with the light
                // or the theme -- a sprite NAME, an offset shape and a filter
                // shape are all properties of the document -- so the same
                // count must come back from all twelve combinations.
                assert_capability_degradations(&capability);
                // The only known approximations on this fixture, per the
                // CORRECT `calculateLightsBrightness` formula (fix round 2
                // of the task-2 report -- this exception used to be
                // "dusk"/"default"+"faded" under the earlier, wrong
                // approximation, whose folded brightness for THOSE combos
                // happened to land above this domain instead, once the real
                // formula replaced it): dawn's brightness (0.39614) lands
                // strictly inside recreation-polygon's fill-color domain
                // [0.1, 0.4] -- and inside `landuse`'s, which is the same
                // domain and the same shape -- under the "default" and
                // "faded" themes, but not "monochrome", whose directional-
                // light intensity and colour for dawn differ enough (see
                // the fixture's own `lights`) that its brightness
                // (0.414247) clamps just above the domain instead of
                // landing inside it. `landuse` is the newer of the two: it
                // only reaches the folder at all now that a `["config", k]`
                // filter operand compiles. Pin the exact set rather than
                // just its size, so a different property starting to
                // degrade is caught even if the count stays 2.
                let expect_between_stops_degradations = preset == "dawn" && (theme == "default" || theme == "faded");
                if expect_between_stops_degradations {
                    assert_eq!(
                        degraded.len(),
                        2,
                        "lightPreset {preset:?} + theme {theme:?}: expected exactly the two known \
                         nearest-stop approximations, got:\n{}",
                        degraded.join("\n")
                    );
                    // Both are the same mechanism on the same input, in two
                    // layers: `landuse` (which only reaches the compiler at
                    // all now that its config-driven filter folds) and
                    // `recreation-polygon`. Matched by their own `match`
                    // discriminants so a swap for some third layer's
                    // degradation is caught.
                    assert!(
                        degraded.iter().all(|d| d.contains("its input folds to 0.39614")),
                        "lightPreset {preset:?} + theme {theme:?}: both known degradations must be the \
                         between-stops brightness approximation, got:\n{}",
                        degraded.join("\n")
                    );
                    assert!(
                        degraded.iter().any(|d| d.contains(r#"["match",["get","class"],"wood""#)),
                        "lightPreset {preset:?} + theme {theme:?}: missing the landuse fill-color \
                         nearest-stop fallback, got:\n{}",
                        degraded.join("\n")
                    );
                    assert!(
                        degraded.iter().any(|d| d.contains(r#"["match",["get","type"],["parking","playground","skateboard","surface"]"#)),
                        "lightPreset {preset:?} + theme {theme:?}: missing the recreation-polygon fill-color \
                         nearest-stop fallback, got:\n{}",
                        degraded.join("\n")
                    );
                } else {
                    assert!(
                        degraded.is_empty(),
                        "lightPreset {preset:?} + theme {theme:?} degraded {} propert(ies) that this pinned fixture \
                         previously folded exactly:\n{}",
                        degraded.len(),
                        degraded.join("\n")
                    );
                }
            }
        }
    }

    /// The counter-test that keeps the assertion above from being vacuous:
    /// a document that DOES degrade must show up in `take_degradations`.
    #[test]
    fn a_folder_refusal_is_recorded_not_just_printed() {
        // `["config", k]` with no config resolved at all is the folder's
        // simplest refusal, and `or_degrade` keeps the layer.
        let json = br##"{
            "version": 8,
            "layers": [{ "id": "land", "type": "background", "paint": { "background-color": ["config", "colorLand"] } }]
        }"##;
        let _ = take_degradations();
        let style = parse_and_compile(json).expect("a folder refusal degrades rather than failing");
        assert_eq!(layer_ids(&style), vec!["land"], "the layer survives the degradation");
        let degraded = take_degradations();
        assert_eq!(degraded.len(), 1, "{degraded:?}");
        assert!(degraded[0].contains("land"), "the notice names the layer: {}", degraded[0]);
        assert!(degraded[0].contains("background-color"), "the notice names the property: {}", degraded[0]);
        assert!(take_degradations().is_empty(), "taking the notices drains them");
    }

    /// The point of the whole series, asserted on the compiled `Style`
    /// rather than on an expression in isolation: the two layers that cover
    /// the most pixels come out DARK, because `parse_and_compile` resolved
    /// the import's `config` and the imported document's `lights` and
    /// handed both to the layers it compiled.
    ///
    /// Both colours are now the LIT ones. Night + monochrome puts both of
    /// this fixture's lights at black and zero intensity, so the scene's
    /// ground radiance is exactly `[0, 0, 0]` and
    /// `mix(radiance, 1, strength)` collapses to the emissive strength
    /// itself -- every colour here is its authored value times how much of
    /// itself the style says it emits.
    #[test]
    fn the_vendored_dark_fixtures_land_and_water_compile_to_dark_colours() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let style = parse_and_compile(&bytes).expect("the fixture must compile");

        let land = style
            .layers
            .iter()
            .find_map(|l| match l {
                StyleLayer::Background { id, color, .. } if id == "land" => Some(color),
                _ => None,
            })
            .expect("the imported basemap's background must compile");
        // colorLand is not overridden, so its schema default resolves:
        // hsl(20, 20%, 95%) -- which is what the let/at/to-hsla/hsl
        // decomposition rebuilds -- and `background-emissive-strength` is
        // 0.1 everywhere below zoom 13.
        // ...and then graded by the import's own `theme: monochrome`
        // `color-theme` before the lighting multiply, which is what turns
        // that warm off-white into a neutral 0.986 grey.
        assert_rgba_close(land.value_at(0), scale_rgb(graded::LAND, 0.1), "land z0");

        let water = style
            .layers
            .iter()
            .find_map(|l| match l {
                StyleLayer::Fill { id, color, .. } if id == "water" => Some(color),
                _ => None,
            })
            .expect("the imported basemap's water must compile");
        // Night + monochrome: brightness 0 takes the darkened branch,
        // colorWater's override hsl(0, 0%, 76%) less 55 points of lightness.
        // That darkening is itself a zoom ramp (colorWater straight through
        // up to zoom 13, the darkened branch from zoom 14), and
        // `fill-emissive-strength` is a second, independent ramp: 0.05 at
        // zoom 5 down to 0 at zoom 7 and flat after. Both are visible here,
        // multiplied together -- and by zoom 14 the water emits nothing at
        // all into a scene with no light in it, so it is pure black, which
        // is exactly what the reference draws for it.
        assert_rgba_close(water.value_at(5), scale_rgb(graded::WATER, 0.05), "water z5");

        // Deliberately NOT written as `darkened_colour * 0.0`, which any
        // colour whatsoever would satisfy: what is asserted here is only
        // that the water is black. The darkened colour ITSELF is pinned by
        // `waters_darkening_branch_survives_compilation_under_a_light_that_reaches_it`,
        // which reaches it with a light that does not multiply it away.
        assert_rgba_close(water.value_at(14), Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }, "water z14");
    }


    // ---- the `color-theme` LUT (colour grading) -------------------------
    //
    // `RenderColor`'s constructor in `src/style-spec/util/color.ts`
    // (Mapbox GL JS 3.15.0), applied on the CPU when a paint colour is read
    // and BEFORE the shader's lighting. See `style_spec::lut` for the
    // arithmetic and `themed_and_lit_color` for the ordering.

    /// A `color-theme.data` payload: the base64 PNG of an `N x N*N` cube
    /// built by evaluating `grade` at every grid point.
    ///
    /// The style-document form of `lut::tests::cube`, so these tests drive
    /// the grade the way a real style does -- through `parse_and_compile`,
    /// base64 and PNG decode included -- rather than by handing a
    /// `ColorLut` straight to the sampler.
    fn color_theme_payload(size: usize, grade: impl Fn(f32, f32, f32) -> (f32, f32, f32)) -> String {
        use base64::Engine as _;
        let last = (size - 1) as f32;
        let mut data = vec![0u8; size * size * size * 4];
        for blue in 0..size {
            for green in 0..size {
                for red in 0..size {
                    let (r, g, b) = grade(red as f32 / last, green as f32 / last, blue as f32 / last);
                    let at = (red + green * size * size + blue * size) * 4;
                    data[at] = (r * 255.0).round() as u8;
                    data[at + 1] = (g * 255.0).round() as u8;
                    data[at + 2] = (b * 255.0).round() as u8;
                    data[at + 3] = 255;
                }
            }
        }
        let image = image::RgbaImage::from_raw((size * size) as u32, size as u32, data).expect("a well-shaped image");
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encodes");
        base64::engine::general_purpose::STANDARD.encode(&png)
    }

    /// A 2-sample cube is the one identity that is EXACT. Its only grid
    /// points are 0 and 255, so the trilinear weight is the input channel
    /// itself and `lerp(0, 255, c) / 255 == c` for every `c` -- no
    /// quantisation anywhere. (A larger identity cube is not exact: `1/31`
    /// does not survive a round trip through 8 bits.)
    fn identity_color_theme() -> String {
        color_theme_payload(2, |r, g, b| (r, g, b))
    }

    /// Every colour a compiled style holds, in layer order -- the same walk
    /// `colour_digest` does, kept separate so a failure names the colour
    /// that moved instead of printing two different `u64`s.
    fn every_colour(style: &Style) -> Vec<(String, &'static str, u8, Rgba)> {
        let mut all = Vec::new();
        for layer in &style.layers {
            let (id, colours): (&str, Vec<(&'static str, &ZoomValue<Rgba>)>) = match layer {
                StyleLayer::Background { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Fill { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Extrusion { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Line { id, color, .. } => (id, vec![("color", color)]),
                StyleLayer::Symbol { id, text_color, text_halo_color, icon_color, icon_halo_color, .. } => (
                    id,
                    vec![
                        ("text_color", text_color),
                        ("text_halo_color", text_halo_color),
                        ("icon_color", icon_color),
                        ("icon_halo_color", icon_halo_color),
                    ],
                ),
                // No colours: a raster layer's appearance is its texels.
                // See `colours_of` for why that is a property of the layer
                // type rather than an omission here.
                StyleLayer::Raster { id, .. } => (id, vec![]),
                StyleLayer::Circle { id, paint, .. } => (id, vec![("color", &paint.color), ("stroke_color", &paint.stroke_color)]),
            };
            for (field, value) in colours {
                for &(zoom, colour) in &value.stops {
                    all.push((id.to_string(), field, zoom, colour));
                }
            }
        }
        all
    }

    /// ACCEPTANCE: an identity grade leaves every colour in a real style
    /// where it was.
    ///
    /// "Where it was" and not "to the bit": the sampler is the reference's
    /// own `(a * (1 - t)) + (b * t)` (`style-spec/util/interpolate.ts`),
    /// which for `a == b == 255` is not exactly 255 in `f32` at every `t`.
    /// The residual is a fraction of an 8-bit count -- three orders of
    /// magnitude below anything that can reach a pixel -- and closing it
    /// would mean using a lerp the reference does not.
    ///
    /// This is the test that says the plumbing itself costs nothing. It
    /// would pass vacuously if the LUT were never reached at all (a
    /// misspelled key, a `data` that silently would not fold), so the
    /// counterweight inverts the same cube through the same path and
    /// demands every colour MOVE.
    #[test]
    fn an_identity_color_theme_leaves_every_colour_where_it_was() {
        let bytes = std::fs::read("examples/liberty-style.json").expect("run from the repo root");
        let ungraded = every_colour(&parse_and_compile(&bytes).expect("the fixture must compile"));
        assert!(ungraded.len() > 100, "the fixture must have colours to compare, got {}", ungraded.len());

        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        root["color-theme"] = serde_json::json!({ "data": identity_color_theme() });
        let graded = every_colour(&parse_and_compile(root.to_string().as_bytes()).expect("must compile"));

        assert_eq!(graded.len(), ungraded.len(), "the grade must not change which layers compile");
        for (was, now) in ungraded.iter().zip(graded.iter()) {
            assert_eq!((&was.0, was.1, was.2), (&now.0, now.1, now.2), "the walk must line up");
            // A quarter of one 8-bit count, which cannot reach a pixel.
            assert_rgba_close_within(now.3, was.3, 1e-3, &format!("{} {} at z{}", was.0, was.1, was.2));
        }

        // Not vacuous: the same document with an INVERTING cube in the same
        // place moves every colour it touches.
        root["color-theme"] = serde_json::json!({ "data": color_theme_payload(2, |r, g, b| (1.0 - r, 1.0 - g, 1.0 - b)) });
        let inverted = every_colour(&parse_and_compile(root.to_string().as_bytes()).expect("must compile"));
        let moved = inverted.iter().zip(ungraded.iter()).filter(|(now, was)| (now.3.r - was.3.r).abs() > 0.1).count();
        assert!(moved > ungraded.len() / 2, "an inverting cube must move most colours, moved {moved}/{}", ungraded.len());
    }

    /// ACCEPTANCE, and the one no other test can catch: the LUT runs BEFORE
    /// the lighting multiply, not after.
    ///
    /// Both orders produce colours; both look plausible; and every
    /// expectation written against a single style is satisfied by whichever
    /// order that style was compiled with. So this uses an operation that
    /// does NOT commute with a scalar multiply -- inversion -- and derives
    /// the lighting factor from the style itself rather than pinning it:
    ///
    /// - compiled with an IDENTITY cube, the colour is `c * f`, which gives
    ///   `f`;
    /// - graded-then-lit, it must be `(1 - c) * f`;
    /// - lit-then-graded, it would be `1 - c * f`, i.e. `1 - identity`.
    ///
    /// With `c = 0.5` and any `f` in `(0, 1)` those two are far apart --
    /// the first is below 0.5, the second above it -- so this cannot pass
    /// by accident.
    #[test]
    fn the_lut_is_applied_before_the_lighting_multiply_not_after() {
        // A scene with real lighting: a white ambient at full intensity and
        // a black directional, which `ground_radiance` folds to something
        // strictly between 0 and 1, times an emissive strength of 0.
        let document = |theme: &str| {
            serde_json::json!({
                "version": 8,
                "imports": [{
                    "id": "basemap",
                    "config": {},
                    "data": {
                        "version": 8,
                        "schema": {},
                        "lights": [
                            { "id": "ambient", "type": "ambient", "properties": { "color": "#ffffff", "intensity": 0.5 } },
                            { "id": "directional", "type": "directional", "properties": {
                                "color": "#000000", "intensity": 0.0, "direction": ["literal", [0, 0]] } }
                        ],
                        "color-theme": { "data": theme },
                        "layers": [{
                            "id": "grey",
                            "type": "background",
                            "paint": { "background-color": "rgb(128, 128, 128)", "background-emissive-strength": 0 }
                        }]
                    }
                }],
                "layers": []
            })
            .to_string()
        };
        let grey_at = |json: String| {
            let style = parse_and_compile(json.as_bytes()).expect("must compile");
            match style.layers.iter().find(|l| matches!(l, StyleLayer::Background { id, .. } if id == "grey")) {
                Some(StyleLayer::Background { color, .. }) => color.value_at(0),
                _ => panic!("the background must compile"),
            }
        };

        let unlit = 128.0 / 255.0;
        let identity = grey_at(document(&identity_color_theme()));
        let factor = identity.r / unlit;
        assert!(factor > 0.05 && factor < 0.95, "the light must actually do something, got a factor of {factor}");

        let inverted = grey_at(document(&color_theme_payload(2, |r, g, b| (1.0 - r, 1.0 - g, 1.0 - b))));
        let graded_then_lit = (1.0 - unlit) * factor;
        let lit_then_graded = 1.0 - identity.r;
        assert!(
            (inverted.r - graded_then_lit).abs() < 1e-5,
            "the grade must run first: got {}, want {graded_then_lit} (grading the LIT colour would give {lit_then_graded})",
            inverted.r
        );
        assert!(
            (inverted.r - lit_then_graded).abs() > 0.2,
            "and the two orders must be far enough apart for this to mean something: {} vs {lit_then_graded}",
            inverted.r
        );
    }

    /// `*-color-use-theme: "none"` opts one property out of the grade, as
    /// `draw_background.ts`'s `ignoreLut` does. Nothing in either vendored
    /// fixture uses it, so the pin is synthetic -- but a style that asked to
    /// be left alone and was graded anyway would be a wrong colour with
    /// nothing on screen to explain it.
    #[test]
    fn a_color_use_theme_of_none_opts_that_property_out_of_the_grade() {
        let document = |use_theme: Value| {
            let mut paint = serde_json::json!({ "background-color": "rgb(255, 0, 0)" });
            if !use_theme.is_null() {
                paint["background-color-use-theme"] = use_theme;
            }
            serde_json::json!({
                "version": 8,
                "color-theme": { "data": color_theme_payload(2, |r, g, b| (1.0 - r, 1.0 - g, 1.0 - b)) },
                "layers": [{ "id": "bg", "type": "background", "paint": paint }]
            })
            .to_string()
        };
        let red_at = |json: String| {
            let style = parse_and_compile(json.as_bytes()).expect("must compile");
            match &style.layers[0] {
                StyleLayer::Background { color, .. } => color.value_at(0),
                other => panic!("expected a background, got {other:?}"),
            }
        };

        // Graded: the inverting cube turns red into cyan.
        let graded = red_at(document(Value::Null));
        assert!(graded.r < 0.01 && graded.g > 0.99, "the inverting cube must reach it, got {graded:?}");

        // `'none'` opts out; `'default'` (and anything else) does not.
        let opted_out = red_at(document(serde_json::json!("none")));
        assert!(opted_out.r > 0.99 && opted_out.g < 0.01, "\"none\" must leave it red, got {opted_out:?}");
        let defaulted = red_at(document(serde_json::json!("default")));
        assert!(defaulted.r < 0.01, "\"default\" must still be graded, got {defaulted:?}");
    }

    /// ACCEPTANCE, and the reason the whole thing exists. Standard's
    /// `admin-0-boundary` really is pink -- its `line-color` is
    /// `["config", "colorAdminBoundaries"]`, which the fixture overrides to
    /// `hsl(345, 100%, 70%)` -- and Mapbox renders it grey, because
    /// `theme: monochrome` grades it on the way to the screen.
    ///
    /// Measured as HSV saturation, `(max - min) / max`, which is invariant
    /// under the lighting multiply (a uniform per-channel scale) so the
    /// compiled, already-lit colour can be measured directly.
    ///
    /// The drop is asserted as a NUMBER, and against the same fixture with
    /// its `color-theme` removed rather than against a remembered constant:
    /// ungraded it is 0.6 saturated, graded it is flat neutral.
    #[test]
    fn the_monochrome_theme_desaturates_the_fixtures_admin_boundaries() {
        fn saturation(color: Rgba) -> f32 {
            let max = color.r.max(color.g).max(color.b);
            let min = color.r.min(color.g).min(color.b);
            if max <= 0.0 {
                0.0
            } else {
                (max - min) / max
            }
        }
        let admin_0 = |style: &crate::style::Style| {
            style
                .layers
                .iter()
                .find_map(|l| match l {
                    StyleLayer::Line { id, color, .. } if id == "admin-0-boundary" => Some(color.value_at(0)),
                    _ => None,
                })
                .expect("the imported basemap's admin-0-boundary must compile")
        };

        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        root["imports"][0]["data"].as_object_mut().unwrap().remove("color-theme");
        let ungraded = admin_0(&parse_and_compile(root.to_string().as_bytes()).expect("must compile"));
        let graded = admin_0(&parse_and_compile(&bytes).expect("must compile"));

        let before = saturation(ungraded);
        let after = saturation(graded);
        // `hsl(345, 100%, 70%)` is `(1.0, 0.4, 0.55)`: saturation 0.6.
        assert!((before - 0.6).abs() < 1e-3, "ungraded it is the config's pink, saturation {before}");
        assert!(after < 0.02, "graded it must be neutral, saturation {after} (from {before})");
        assert!(before - after > 0.58, "the drop must be at least 0.58, got {}", before - after);
        // And it is not merely desaturated: `monochrome` is a grade, so the
        // grey it lands on is its own value, not the pink's luminance.
        assert!(graded.r > 0.5 && graded.r < 0.65, "a mid grey, got {graded:?}");
    }

    /// The `color-theme` is per-DOCUMENT where the lighting is scene-global:
    /// in the reference each fragment `Style` grades only its own layers
    /// (`Style._reloadColorTheme` walks `this._layers`), so a root-document
    /// layer is lit by the import's lights and graded by the ROOT's theme.
    ///
    /// The Standard fixture is exactly this shape -- a themed import under
    /// an unthemed root -- and getting it wrong would grade its buildings,
    /// aeroways and tunnels with a theme their document never asked for.
    #[test]
    fn a_root_documents_layers_are_graded_by_the_roots_theme_not_the_imports() {
        let json = serde_json::json!({
            "version": 8,
            "imports": [{
                "id": "basemap",
                "config": {},
                "data": {
                    "version": 8,
                    "schema": {},
                    "color-theme": { "data": color_theme_payload(2, |r, g, b| (1.0 - r, 1.0 - g, 1.0 - b)) },
                    "layers": [{ "id": "imported", "type": "background", "paint": { "background-color": "rgb(255, 0, 0)" } }]
                }
            }],
            "layers": [{ "id": "root", "type": "background", "paint": { "background-color": "rgb(255, 0, 0)" } }]
        })
        .to_string();
        let style = parse_and_compile(json.as_bytes()).expect("must compile");
        let color = |want: &str| {
            style
                .layers
                .iter()
                .find_map(|l| match l {
                    StyleLayer::Background { id, color, .. } if id == want => Some(color.value_at(0)),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no background {want:?}"))
        };
        let imported = color("imported");
        assert!(imported.r < 0.01 && imported.g > 0.99, "the import's own theme grades its layers, got {imported:?}");
        let root = color("root");
        assert!(root.r > 0.99 && root.g < 0.01, "the root declares no theme, so its layers are ungraded, got {root:?}");
    }

    /// A `color-theme` whose payload will not decode degrades loudly and
    /// leaves the style ungraded, rather than failing the compile or
    /// silently drawing the wrong colours.
    #[test]
    fn an_undecodable_color_theme_degrades_loudly_and_grades_nothing() {
        let json = serde_json::json!({
            "version": 8,
            "color-theme": { "data": "not-a-png" },
            "layers": [{ "id": "bg", "type": "background", "paint": { "background-color": "rgb(255, 0, 0)" } }]
        })
        .to_string();
        let _ = take_degradations();
        let style = parse_and_compile(json.as_bytes()).expect("a broken theme must not fail the style");
        let StyleLayer::Background { color, .. } = &style.layers[0] else { panic!("expected a background") };
        assert!(color.value_at(0).r > 0.99, "the colour survives ungraded");
        let degraded = take_degradations();
        assert_eq!(degraded.len(), 1, "{degraded:?}");
        assert!(degraded[0].contains("color-theme"), "the notice names the property: {}", degraded[0]);
    }

    /// The empty payload is Standard's own `"default"` theme, and the
    /// `match` fallback for an unknown one: it means "no grade", not "a
    /// broken theme". Nothing is degraded and nothing is graded.
    #[test]
    fn an_empty_color_theme_payload_is_no_grade_at_all_and_no_degradation() {
        let json = serde_json::json!({
            "version": 8,
            "color-theme": { "data": "" },
            "layers": [{ "id": "bg", "type": "background", "paint": { "background-color": "rgb(255, 0, 0)" } }]
        })
        .to_string();
        let _ = take_degradations();
        let style = parse_and_compile(json.as_bytes()).expect("must compile");
        let StyleLayer::Background { color, .. } = &style.layers[0] else { panic!("expected a background") };
        assert!(color.value_at(0).r > 0.99, "ungraded");
        assert!(take_degradations().is_empty(), "an empty payload is not a degradation");
    }

    // ---- import composition -------------------------------------------

    /// A Mapbox Standard style's real basemap is not in its own `layers`
    /// array at all: it arrives through `imports[0].data`, a complete
    /// nested style document. Compiling only the top-level array (which is
    /// all `parse_and_compile` used to do) draws the overlay and nothing
    /// underneath it.
    #[test]
    fn an_imports_entrys_layers_compile_beneath_the_top_level_ones() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8, "layers": [
                { "id": "imported-land", "type": "background", "paint": { "background-color": "#111111" } },
                { "id": "imported-water", "type": "fill", "source-layer": "water", "paint": { "fill-color": "#000022" } }
            ] } }],
            "layers": [
                { "id": "overlay", "type": "line", "source-layer": "road", "paint": { "line-color": "#ff0000" } }
            ]
        }"##;
        let style = parse_and_compile(json).expect("an imported document must compile");
        assert_eq!(
            layer_ids(&style),
            vec!["imported-land", "imported-water", "overlay"],
            "the imported stack is the BASE: it draws first, with the top-level layers over it"
        );
    }

    /// The whole point of `config`: an imported layer's `["config", k]`
    /// only resolves if `parse_and_compile` attaches the import's own
    /// `ResolvedConfig` to the scope it compiles that layer in. Without
    /// that wiring the key is unresolvable, the property degrades to the
    /// literal fallback, and the map is black.
    #[test]
    fn an_imported_layer_resolves_config_against_its_own_import_entry() {
        let json = br##"{
            "version": 8,
            "imports": [{
                "id": "basemap",
                "config": { "colorLand": "#0a0b0c" },
                "data": {
                    "version": 8,
                    "schema": { "colorLand": { "default": "#ffffff" }, "colorWater": { "default": "#010203" } },
                    "layers": [
                        { "id": "land", "type": "background", "paint": { "background-color": ["config", "colorLand"] } },
                        { "id": "water", "type": "fill", "source-layer": "water", "paint": { "fill-color": ["config", "colorWater"] } }
                    ]
                }
            }],
            "layers": []
        }"##;
        let style = parse_and_compile(json).expect("an imported document must compile");
        assert_eq!(layer_ids(&style), vec!["land", "water"]);
        let StyleLayer::Background { color, .. } = &style.layers[0] else { panic!("land is a background layer") };
        // The import entry's own config wins over the schema default.
        assert_eq!(color.value_at(0), parse_css_color("#0a0b0c").unwrap());
        let StyleLayer::Fill { color, .. } = &style.layers[1] else { panic!("water is a fill layer") };
        // No override for this one: the schema default is what resolves.
        assert_eq!(color.value_at(0), parse_css_color("#010203").unwrap());
    }

    /// `measure-light` reads the IMPORTED document's `lights`, which lives
    /// beside its `layers` in the same `data` object. Same wiring question
    /// as `config`, different field -- and this is the one that decides
    /// whether a style asking for night renders dark.
    ///
    /// Both lights are black at zero intensity: `measure-light` requires
    /// BOTH `ambient` and `directional` to be present (see
    /// `measure_light_brightness`'s doc comment on the both-lights gate),
    /// so a lone `ambient` entry is not enough, and both being black gives
    /// brightness 0 -- the dark stop -- as well as a ground radiance of
    /// exactly `[0, 0, 0]` (same shape as the `black_lights` helper below).
    /// `water`'s `fill-emissive-strength: 1` is what keeps THIS test about
    /// `measure-light`'s branch selection rather than that unrelated
    /// zero-radiance multiplication: `mix(color * ground_radiance, color,
    /// 1)` collapses to plain, unlit `color`, which is what the assertion
    /// below actually pins.
    #[test]
    fn an_imported_layer_resolves_measure_light_against_the_imported_documents_lights() {
        let json = br##"{
            "version": 8,
            "imports": [{
                "id": "basemap",
                "data": {
                    "version": 8,
                    "lights": [
                        { "id": "ambient", "type": "ambient", "properties": { "color": "#000000", "intensity": 0 } },
                        { "id": "directional", "type": "directional", "properties": {
                            "color": "#000000", "intensity": 0, "direction": ["literal", [0, 0]]
                        } }
                    ],
                    "layers": [
                        { "id": "water", "type": "fill", "source-layer": "water",
                          "paint": { "fill-color": ["interpolate", ["linear"], ["measure-light", "brightness"],
                                                    0, "#001020", 0.02, "#a0c0ff"],
                                     "fill-emissive-strength": 1 } }
                    ]
                }
            }],
            "layers": []
        }"##;
        let style = parse_and_compile(json).expect("an imported document must compile");
        let StyleLayer::Fill { color, .. } = &style.layers[0] else { panic!("water is a fill layer") };
        assert_eq!(color.value_at(0), parse_css_color("#001020").unwrap());
    }

    /// An import whose own document imports further documents is a style
    /// this compiler does not compose, and the failure mode of quietly
    /// ignoring it is EXACTLY the bug this whole series exists to fix -- a
    /// basemap silently missing from a document that plainly declares one.
    /// So it is a hard error naming the import, not a skip.
    #[test]
    fn an_import_whose_data_itself_has_imports_is_a_hard_error() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": {
                "version": 8,
                "imports": [{ "id": "nested", "data": { "version": 8, "layers": [] } }],
                "layers": []
            } }],
            "layers": []
        }"##;
        let error = parse_and_compile(json).unwrap_err().to_string();
        assert!(error.contains("basemap"), "the error must name the import: {error}");
        assert!(error.contains("imports"), "the error must say what it could not do: {error}");
    }

    /// An `imports` entry whose `data` carries no `layers` array is a
    /// fragment with nothing to draw, not a broken document.
    #[test]
    fn an_import_with_no_layers_array_contributes_nothing_and_is_not_an_error() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8 } }],
            "layers": [{ "id": "overlay", "type": "background", "paint": { "background-color": "#ffffff" } }]
        }"##;
        let style = parse_and_compile(json).expect("a layerless import must not fail the style");
        assert_eq!(layer_ids(&style), vec!["overlay"]);
    }

    /// Several imports compose bottom-to-top in declaration order, each
    /// against its OWN resolved config -- two fragments' `["config", k]`
    /// for the same key must not resolve to the same value.
    #[test]
    fn several_imports_compose_in_order_each_against_its_own_config() {
        let json = br##"{
            "version": 8,
            "imports": [
                { "id": "under", "config": { "c": "#111111" },
                  "data": { "version": 8, "schema": { "c": { "default": "#000000" } },
                            "layers": [{ "id": "under-fill", "type": "fill", "source-layer": "a", "paint": { "fill-color": ["config", "c"] } }] } },
                { "id": "over", "config": { "c": "#222222" },
                  "data": { "version": 8, "schema": { "c": { "default": "#000000" } },
                            "layers": [{ "id": "over-fill", "type": "fill", "source-layer": "b", "paint": { "fill-color": ["config", "c"] } }] } }
            ],
            "layers": [{ "id": "overlay", "type": "background", "paint": { "background-color": "#ffffff" } }]
        }"##;
        let style = parse_and_compile(json).expect("two imports must compile");
        assert_eq!(layer_ids(&style), vec!["under-fill", "over-fill", "overlay"]);
        let StyleLayer::Fill { color, .. } = &style.layers[0] else { panic!() };
        assert_eq!(color.value_at(0), parse_css_color("#111111").unwrap());
        let StyleLayer::Fill { color, .. } = &style.layers[1] else { panic!() };
        assert_eq!(color.value_at(0), parse_css_color("#222222").unwrap());
    }

    /// A top-level layer is NOT compiled against any import's config: the
    /// root document has no `schema` of its own, and `config` is scoped to
    /// the fragment that declares it. Degrading rather than resolving is
    /// the correct answer here.
    #[test]
    fn a_top_level_layer_does_not_read_an_imports_config() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "config": { "colorLand": "#0a0b0c" },
                          "data": { "version": 8, "schema": { "colorLand": { "default": "#ffffff" } }, "layers": [] } }],
            "layers": [{ "id": "overlay", "type": "background", "paint": { "background-color": ["config", "colorLand"] } }]
        }"##;
        let _ = take_degradations();
        let style = parse_and_compile(json).expect("the layer degrades rather than failing the style");
        let StyleLayer::Background { color, .. } = &style.layers[0] else { panic!() };
        // `["config", "colorLand"]` refuses to fold with no config resolved
        // for a top-level layer, so `or_degrade` keeps the layer and falls
        // back to `background-color`'s own literal default (black) --
        // neither the import's override (#0a0b0c) nor its schema default
        // (#ffffff), which `assert_ne!(color, "#0a0b0c")` alone could not
        // tell apart from any other wrong answer.
        assert_eq!(color.value_at(0), parse_css_color("#000000").unwrap());
        let degraded = take_degradations();
        assert_eq!(degraded.len(), 1, "{degraded:?}");
        assert!(degraded[0].contains("overlay"), "the notice names the layer: {}", degraded[0]);
        assert!(degraded[0].contains("background-color"), "the notice names the property: {}", degraded[0]);
    }

    // ---- slot placement -------------------------------------------------

    /// The whole point of `slot`: a top-level layer naming a marker the
    /// import declares interleaves INTO the imported stack at that marker,
    /// rather than drawing over the entire thing the way `slot: ""` does.
    /// This fixture's `bottom` marker sits between the import's polygons
    /// and its lines, matching the real Standard document's own layout
    /// (`examples/mapbox-standard-dark-style.json`: land/water fills, then
    /// the `bottom` marker, then the tunnel/road line layers).
    #[test]
    fn a_slot_bottom_layer_lands_above_the_imports_polygons_and_below_its_lines() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8, "layers": [
                { "id": "land", "type": "background", "paint": { "background-color": "#101010" } },
                { "id": "landuse", "type": "fill", "source-layer": "landuse", "paint": { "fill-color": "#202020" } },
                { "id": "bottom", "type": "slot" },
                { "id": "road", "type": "line", "source-layer": "road", "paint": { "line-color": "#303030" } },
                { "id": "middle", "type": "slot" },
                { "id": "building", "type": "fill", "source-layer": "building", "paint": { "fill-color": "#404040" } },
                { "id": "top", "type": "slot" }
            ] } }],
            "layers": [
                { "id": "water", "type": "fill", "source-layer": "water", "slot": "bottom", "paint": { "fill-color": "#505050" } }
            ]
        }"##;
        let style = parse_and_compile(json).expect("a slotted layer must compile");
        let ids = layer_ids(&style);
        let land = ids.iter().position(|&id| id == "land").expect("imported land must compile");
        let landuse = ids.iter().position(|&id| id == "landuse").expect("imported landuse must compile");
        let water = ids.iter().position(|&id| id == "water").expect("the slotted layer must compile");
        let road = ids.iter().position(|&id| id == "road").expect("imported road must compile");
        assert!(land < water && landuse < water, "slot:\"bottom\" sits above the imported polygons: {ids:?}");
        assert!(water < road, "slot:\"bottom\" sits below the imported lines: {ids:?}");
    }

    /// Task 3's guarantee, still true with slot markers in the mix: an
    /// unslotted (`slot: ""`) layer draws above the ENTIRE import, not just
    /// above the last-declared marker.
    #[test]
    fn an_empty_slot_still_lands_above_the_entire_import_when_markers_are_present() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8, "layers": [
                { "id": "land", "type": "background", "paint": { "background-color": "#101010" } },
                { "id": "bottom", "type": "slot" },
                { "id": "road", "type": "line", "source-layer": "road", "paint": { "line-color": "#202020" } },
                { "id": "top", "type": "slot" },
                { "id": "label-halo", "type": "fill", "source-layer": "poi", "paint": { "fill-color": "#303030" } }
            ] } }],
            "layers": [
                { "id": "overlay", "type": "background", "paint": { "background-color": "#ffffff" } }
            ]
        }"##;
        let style = parse_and_compile(json).expect("must compile");
        assert_eq!(
            layer_ids(&style),
            vec!["land", "road", "label-halo", "overlay"],
            "slot: \"\" still draws above every imported layer, marker or not"
        );
    }

    /// The style spec's documented answer for a `slot` value that names no
    /// marker the import declares: treat it exactly like `slot: ""`, i.e.
    /// place the layer on top of the whole style. Checked against Mapbox's
    /// own docs rather than assumed: the style spec's slots reference
    /// (docs.mapbox.com/style-spec/reference/slots/) states "Without the
    /// `slot` property, custom layers render on top of all imported
    /// layers", and Mapbox's own guidance elsewhere is explicit that
    /// referencing a slot the style doesn't declare gets the same
    /// treatment -- the layer lands on top of the style by default. There
    /// is no more sensible fallback here either way, since a name that
    /// matches nothing gives this compiler no OTHER position to prefer,
    /// and silently dropping the layer would contradict this module's own
    /// "nothing is ever dropped" guarantee for slot placement.
    #[test]
    fn an_unrecognised_slot_name_is_treated_like_unslotted_and_lands_on_top() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8, "layers": [
                { "id": "land", "type": "background", "paint": { "background-color": "#101010" } },
                { "id": "bottom", "type": "slot" },
                { "id": "road", "type": "line", "source-layer": "road", "paint": { "line-color": "#202020" } }
            ] } }],
            "layers": [
                { "id": "overlay", "type": "background", "slot": "no-such-marker", "paint": { "background-color": "#ffffff" } }
            ]
        }"##;
        let style = parse_and_compile(json).expect("must compile");
        assert_eq!(
            layer_ids(&style),
            vec!["land", "road", "overlay"],
            "an unrecognised slot name lands on top, same as slot: \"\""
        );
    }

    /// `type: "slot"` layers are pure insertion markers: they must never
    /// themselves produce a `StyleLayer`, whether or not any top-level
    /// layer actually targets them.
    #[test]
    fn slot_marker_layers_contribute_no_style_layer() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8, "layers": [
                { "id": "land", "type": "background", "paint": { "background-color": "#101010" } },
                { "id": "bottom", "type": "slot" },
                { "id": "middle", "type": "slot" },
                { "id": "top", "type": "slot" }
            ] } }],
            "layers": []
        }"##;
        let style = parse_and_compile(json).expect("must compile");
        assert_eq!(layer_ids(&style), vec!["land"], "the three slot markers contribute nothing");
    }

    /// Two top-level layers naming the SAME slot must land in the order
    /// they appear in the document, not be reordered or collapsed.
    #[test]
    fn two_layers_targeting_the_same_slot_preserve_their_document_order() {
        let json = br##"{
            "version": 8,
            "imports": [{ "id": "basemap", "data": { "version": 8, "layers": [
                { "id": "land", "type": "background", "paint": { "background-color": "#101010" } },
                { "id": "bottom", "type": "slot" },
                { "id": "road", "type": "line", "source-layer": "road", "paint": { "line-color": "#202020" } }
            ] } }],
            "layers": [
                { "id": "first", "type": "fill", "source-layer": "a", "slot": "bottom", "paint": { "fill-color": "#111111" } },
                { "id": "second", "type": "fill", "source-layer": "b", "slot": "bottom", "paint": { "fill-color": "#222222" } }
            ]
        }"##;
        let style = parse_and_compile(json).expect("must compile");
        assert_eq!(layer_ids(&style), vec!["land", "first", "second", "road"]);
    }

    /// The real vendored fixture's acceptance case: one of its 10
    /// `slot: "bottom"` root layers (`"water shadow"`, an id that collides
    /// with nothing on either side, unlike `"water"` or `"landuse"` which
    /// the root document reuses to override the import) must land above
    /// the imported polygon layers and below the imported line layers --
    /// exactly the relative-order guarantee `layer_index` needs for both
    /// draw order and (since `7084ea0`) collision priority.
    ///
    /// Measured, not assumed, and worth recording here: all 10 of the
    /// fixture's `slot: "bottom"` layers -- `"water shadow"` included --
    /// carry `layout.visibility: "none"` and so compile to zero
    /// `StyleLayer`s regardless of slot placement (Mapbox Standard's dark
    /// style apparently keeps these as dormant override points). The
    /// unmodified fixture therefore never exercises named-slot placement at
    /// all -- flip that one layer's visibility on here, the same way the
    /// zero-degradations test below mutates `imports[0].config` in place,
    /// to actually exercise it against the real document rather than only
    /// against a hand-built one.
    #[test]
    fn the_vendored_fixtures_slot_bottom_layer_lands_between_imported_polygons_and_lines() {
        let bytes = mapbox_fixture_or_skip!("mapbox-standard-dark-style.json");
        let mut root: Value = serde_json::from_slice(&bytes).unwrap();
        let mut found = false;
        for layer in root["layers"].as_array_mut().unwrap() {
            if layer.get("id").and_then(Value::as_str) == Some("water shadow") {
                layer["layout"]["visibility"] = serde_json::json!("visible");
                found = true;
            }
        }
        assert!(found, "fixture shape moved out from under this test: no \"water shadow\" layer");
        let document = serde_json::to_vec(&root).unwrap();

        let style = parse_and_compile(&document).expect("the fixture must compile");
        let ids = layer_ids(&style);
        let land = ids.iter().position(|&id| id == "land").expect("imported land must compile");
        let inserted = ids.iter().position(|&id| id == "water shadow").expect("the slot:\"bottom\" root layer must compile");
        let tunnel_path_case =
            ids.iter().position(|&id| id == "tunnel-path-case").expect("an imported line layer must compile");
        assert!(land < inserted, "slot:\"bottom\" sits above the imported polygons: land={land} inserted={inserted}");
        assert!(
            inserted < tunnel_path_case,
            "slot:\"bottom\" sits below the imported lines: inserted={inserted} tunnel_path_case={tunnel_path_case}"
        );
    }

    #[test]
    fn the_streets_arrow_layers_resolve_their_stepped_icon_on_both_sides_of_the_step() {
        let bytes = mapbox_fixture_or_skip!("mapbox-streets-style.json");
        let style = parse_and_compile(&bytes).expect("must compile");
        let layer = style
            .layers
            .iter()
            .find_map(|l| match l {
                StyleLayer::Symbol { id, icon_image, .. } if id == "road-oneway-arrow-blue" => icon_image.as_ref(),
                _ => None,
            })
            .expect("road-oneway-arrow-blue must compile with an icon");
        let tags = std::collections::HashMap::new();
        assert_eq!(layer.resolve(&tags, 17).as_deref(), Some("oneway-small"));
        assert_eq!(layer.resolve(&tags, 18).as_deref(), Some("oneway-large"));
    }


    /// A style with no `sky` gets our defaults -- MapLibre's spec defaults
    /// with a pale-blue horizon -- and both vendored styles are such a style;
    /// a style that declares `"sky": {}` gets the spec's exactly.
    #[test]
    fn a_style_without_a_sky_gets_our_defaults_and_an_empty_sky_the_specs() {
        let style = parse_and_compile(br#"{"version": 8, "layers": []}"#).expect("compiles");
        let sky = style.sky.at_zoom(5.0);
        assert_eq!(sky, crate::style::Sky::default().at_zoom(5.0));
        assert_eq!(sky.sky_color, crate::style::SKY_COLOR_DEFAULT);
        assert_eq!(sky.atmosphere_blend, 0.8);
        assert_eq!(sky.sky_horizon_blend, 0.8);
        assert_eq!(sky.horizon_color, crate::style::HORIZON_COLOR_OURS, "no sky: OUR pale-blue horizon");
        let liberty = parse_and_compile(include_bytes!("../../examples/liberty-style.json")).expect("compiles");
        assert_eq!(liberty.sky.at_zoom(5.0), sky, "the vendored Liberty style declares no sky");
        // A style that DECLARES a sky gets the spec for what it leaves out --
        // the white horizon included.
        let empty = parse_and_compile(br#"{"version": 8, "layers": [], "sky": {}}"#).expect("compiles");
        assert_eq!(empty.sky.at_zoom(5.0), crate::style::Sky::spec().at_zoom(5.0));
        assert_eq!(empty.sky.at_zoom(5.0).horizon_color, crate::style::HORIZON_COLOR_DEFAULT);
    }

    /// `sky` is parsed when present: colours, numbers, a zoom
    /// `interpolate` (the spec's own `atmosphere-blend` example), and a
    /// property left out keeps its default.
    #[test]
    fn a_styles_sky_is_parsed_with_defaults_for_what_it_leaves_out() {
        let style = parse_and_compile(
            br##"{"version": 8, "layers": [], "sky": {
                "sky-color": "#199EF3",
                "horizon-color": "rgba(255, 0, 0, 0.5)",
                "sky-horizon-blend": 0.5,
                "atmosphere-blend": ["interpolate", ["linear"], ["zoom"], 0, 1, 10, 1, 12, 0]
            }}"##,
        )
        .expect("compiles");
        let at5 = style.sky.at_zoom(5.0);
        assert!((at5.sky_color.r - 0x19 as f32 / 255.0).abs() < 1e-6 && (at5.sky_color.b - 0xF3 as f32 / 255.0).abs() < 1e-6);
        assert!((at5.horizon_color.r - 1.0).abs() < 1e-6 && (at5.horizon_color.a - 0.5).abs() < 1e-6);
        assert_eq!(at5.sky_horizon_blend, 0.5);
        assert_eq!(at5.atmosphere_blend, 1.0);
        assert!((style.sky.at_zoom(11.0).atmosphere_blend - 0.5).abs() < 1e-6, "interpolates between stops");
        assert_eq!(style.sky.at_zoom(12.0).atmosphere_blend, 0.0);
        // Left out: defaults.
        assert_eq!(at5.fog_color, crate::style::HORIZON_COLOR_DEFAULT);
        assert_eq!((at5.fog_ground_blend, at5.horizon_fog_blend), (0.5, 0.8));
    }

    /// A malformed `sky` property never fails the style: it keeps its
    /// default and says so.
    #[test]
    fn a_malformed_sky_property_keeps_its_default_and_does_not_fail_the_style() {
        let style = parse_and_compile(br#"{"version": 8, "layers": [], "sky": {"sky-color": 7}}"#).expect("compiles");
        assert_eq!(style.sky.at_zoom(0.0).sky_color, crate::style::SKY_COLOR_DEFAULT);
        assert!(last_compile_diagnostics().iter().any(|d| d.detail.contains("sky.sky-color")));
    }

    // -----------------------------------------------------------------
    // The dropped label layers: one test per newly compiled shape, and the
    // zero-skip guarantee for both vendored Mapbox styles.
    // -----------------------------------------------------------------

    fn numbers(pairs: &[(&str, f64)]) -> HashMap<String, TagValue> {
        pairs.iter().map(|(k, v)| (k.to_string(), TagValue::Number(*v))).collect()
    }

    /// Both vendored Mapbox styles compile with no skipped layer but the
    /// four Streets layers that are not labels and need a renderer feature
    /// this crate does not have -- each named with its reason. Light has
    /// none. Before this change Light skipped 6 layers and Streets 9, every
    /// extra one a label layer.
    #[test]
    fn both_vendored_mapbox_styles_compile_with_only_allow_listed_skips() {
        const STREETS_ALLOWED: &[(&str, &str)] = &[
            // `fill-color` is an `interpolate` over the PROPERTY `min_depth`:
            // a continuous ramp per feature, which the fill path's
            // enumerate-into-layers expansion cannot hold.
            ("water-depth", "fill-color is a property interpolate"),
            // `fill-pattern` only: pattern fills are not implemented.
            ("road-pedestrian-polygon-pattern", "pattern fill"),
            // `circle` is not an implemented layer type.
            ("turning-feature-outline", "circle layer"),
            ("turning-feature", "circle layer"),
        ];
        for (path, allowed) in
            [("demos/mapbox-light-style.json", &[][..]), ("examples/mapbox-streets-style.json", STREETS_ALLOWED)]
        {
            let Some(bytes) = crate::mapbox_fixtures::read(path) else { continue };
            parse_and_compile(&bytes).expect("compiles");
            let mut skipped: Vec<String> = last_compile_diagnostics()
                .into_iter()
                .filter(|d| d.kind == DiagnosticKind::SkippedLayer)
                .filter_map(|d| d.layer)
                .collect();
            skipped.sort();
            let mut expected: Vec<String> = allowed.iter().map(|(id, _)| id.to_string()).collect();
            expected.sort();
            assert_eq!(skipped, expected, "{path}: skipped layers");
        }
    }

    /// `settlement-major-label`'s real `text-color`: a `step` on the
    /// PROPERTY `symbolrank`. Buckets by `>=`, a missing tag in the base.
    #[test]
    fn a_property_step_colour_buckets_by_the_numeric_tag() {
        let json = serde_json::json!(
            ["step", ["get", "symbolrank"], "hsl(220, 1%, 49%)", 11, "hsl(220, 1%, 62%)", 16, "hsl(220, 1%, 71%)"]
        );
        let cases = compile_zoom_value(&json, color_leaf, Scope::root()).expect("a property step compiles");
        let pick = |tags: &HashMap<String, TagValue>| {
            cases.iter().find(|(f, _)| f.matches(tags, 5.0)).map(|(_, v)| v.value_at(5)).expect("a bucket")
        };
        let css = |s: &str| color::parse_css_color(s).unwrap();
        assert_eq!(pick(&numbers(&[("symbolrank", 3.0)])), css("hsl(220, 1%, 49%)"));
        assert_eq!(pick(&numbers(&[("symbolrank", 11.0)])), css("hsl(220, 1%, 62%)"));
        assert_eq!(pick(&numbers(&[("symbolrank", 15.9)])), css("hsl(220, 1%, 62%)"));
        assert_eq!(pick(&numbers(&[("symbolrank", 16.0)])), css("hsl(220, 1%, 71%)"));
        assert_eq!(pick(&HashMap::new()), css("hsl(220, 1%, 49%)"), "a missing tag takes the base");
    }

    /// `country-label`'s real `icon-opacity`: a zoom `step` whose base is a
    /// `case`. Hoisted to a `case` of zoom steps; the feature and zoom
    /// together pick the value.
    #[test]
    fn a_zoom_step_with_a_case_segment_resolves_per_feature() {
        let json = serde_json::json!(["step", ["zoom"], ["case", ["has", "text_anchor"], 1, 0], 7, 0]);
        let opacity = crate::style::SymbolOpacity {
            cases: compile_zoom_value(&json, number_leaf, Scope::root()).expect("compiles"),
        };
        let anchored = tags(&[("text_anchor", "left")]);
        assert_eq!(opacity.resolve(&anchored, 5), 1.0);
        assert_eq!(opacity.resolve(&HashMap::new(), 5), 0.0);
        assert_eq!(opacity.resolve(&anchored, 7), 0.0);
        assert_eq!(opacity.resolve(&HashMap::new(), 9), 0.0);
    }

    /// `water-point-label`'s real `text-size`: a zoom `interpolate` whose
    /// stops are arithmetic over `sizerank`. Each stop evaluated for the
    /// feature, then interpolated by zoom.
    #[test]
    fn a_zoom_interpolate_of_arithmetic_stops_evaluates_per_feature() {
        let json = serde_json::json!(["interpolate", ["linear"], ["zoom"],
            0, ["*", ["-", 16, ["sqrt", ["get", "sizerank"]]], 1],
            22, ["*", ["-", 22, ["sqrt", ["get", "sizerank"]]], 1]]);
        let size = compile_layout_value(&json).expect("compiles").with_fallback(16.0);
        assert!(matches!(size, crate::style::LayoutValue::FeatureExpression { .. }));
        let rank4 = numbers(&[("sizerank", 4.0)]);
        assert_eq!(size.resolve(&rank4, 0), 14.0);
        assert_eq!(size.resolve(&rank4, 22), 20.0);
        assert_eq!(size.resolve(&rank4, 11), 17.0);
        assert_eq!(size.resolve(&numbers(&[("sizerank", 9.0)]), 11), 16.0);
        assert_eq!(size.resolve_pair(&rank4, 11), (17.0, 17.0 + 6.0 / 22.0));
        // `sqrt(null)` is a runtime error in MapLibre: the property default.
        assert_eq!(size.resolve(&HashMap::new(), 11), 16.0);
    }

    /// `transit-label`'s real `text-field`: a zoom `step` over `match`es of
    /// `coalesce`s and `""`.
    #[test]
    fn a_text_field_zoom_step_of_matches_resolves_at_the_bake_zoom() {
        let json = serde_json::json!(["step", ["zoom"], "",
            13, ["match", ["get", "mode"], ["rail", "metro_rail"], ["coalesce", ["get", "name_en"], ["get", "name"]], ""],
            14, ["match", ["get", "mode"], ["bus", "bicycle"], "", ["coalesce", ["get", "name_en"], ["get", "name"]]],
            18, ["coalesce", ["get", "name_en"], ["get", "name"]]]);
        let field = compile_text_field(&json).expect("compiles");
        let rail = tags(&[("mode", "rail"), ("name", "Shinjuku")]);
        let bus = tags(&[("mode", "bus"), ("name", "Stop 4")]);
        assert_eq!(field.resolve_at(&rail, 12), None, "\"\" is no label");
        assert_eq!(field.resolve_at(&rail, 13).as_deref(), Some("Shinjuku"));
        assert_eq!(field.resolve_at(&bus, 13), None);
        assert_eq!(field.resolve_at(&bus, 14), None);
        assert_eq!(field.resolve_at(&bus, 18).as_deref(), Some("Stop 4"));
        assert_eq!(field.resolve_at(&tags(&[("name", "Ferry")]), 14).as_deref(), Some("Ferry"), "no mode: default arm");
    }

    /// `airport-label`'s real `text-field`: a `step` on `sizerank` over a
    /// `case` whose `then` is a `concat` with a nested `coalesce` and a
    /// `"\n"`.
    #[test]
    fn a_text_field_property_step_over_a_nested_concat_resolves() {
        let json = serde_json::json!(["step", ["get", "sizerank"],
            ["case", ["has", "ref"],
                ["concat", ["get", "ref"], " -\n", ["coalesce", ["get", "name_en"], ["get", "name"]]],
                ["coalesce", ["get", "name_en"], ["get", "name"]]],
            15, ["get", "ref"]]);
        let field = compile_text_field(&json).expect("compiles");
        let mut major = tags(&[("ref", "JFK"), ("name_en", "John F. Kennedy")]);
        major.insert("sizerank".into(), TagValue::Number(2.0));
        assert_eq!(field.resolve(&major).as_deref(), Some("JFK -\nJohn F. Kennedy"));
        let mut minor = major.clone();
        minor.insert("sizerank".into(), TagValue::Number(15.0));
        assert_eq!(field.resolve(&minor).as_deref(), Some("JFK"));
        let mut no_ref = tags(&[("name", "Aerodrome")]);
        no_ref.insert("sizerank".into(), TagValue::Number(2.0));
        assert_eq!(field.resolve(&no_ref).as_deref(), Some("Aerodrome"));
    }

    /// `symbol-placement: "line-center"`: one label per line part, at the
    /// part's midpoint, with no `symbol-spacing` repeats -- MapLibre's
    /// `getCenterAnchor`.
    #[test]
    fn line_center_places_one_label_at_each_parts_midpoint() {
        let style = parse_and_compile(br#"{"layers": [{"id": "water-line-label", "type": "symbol",
            "source-layer": "natural_label",
            "layout": {"symbol-placement": "line-center", "text-field": ["get", "name"]}}]}"#)
        .expect("compiles");
        let long = LineString::from(vec![(100, 100), (3100, 100)]);
        let short = LineString::from(vec![(200, 2000), (200, 2400)]);
        let tile = tile_with_feature(
            "natural_label",
            tags(&[("name", "Hudson River")]),
            Geometry::MultiLineString(geo_types::MultiLineString(vec![long, short])),
        );
        let plan = crate::style::match_tile_symbols(&tile, &style, 14, 1.0);
        let mut anchors: Vec<(i32, i32, f64)> =
            plan.features.iter().map(|f| (f.anchor.x(), f.anchor.y(), f.line_centre_distance)).collect();
        anchors.sort_by_key(|a| (a.0, a.1));
        assert_eq!(anchors, vec![(200, 2200, 200.0), (1600, 100, 1500.0)]);
        assert!(plan.features.iter().all(|f| f.line.is_some() && f.text.as_deref() == Some("Hudson River")));
    }

    #[test]
    fn with_3d_buildings_adds_an_extrusion_above_streets_building() {
        let mut doc: serde_json::Value = serde_json::from_slice(&mapbox_fixture_or_skip!("mapbox-streets-style.json")).unwrap();
        with_3d_buildings(&mut doc).expect("Streets has a building layer");
        let ids: Vec<&str> = doc["layers"].as_array().unwrap().iter().map(|l| l["id"].as_str().unwrap()).collect();
        let at = ids.iter().position(|id| *id == "building").unwrap();
        assert_eq!(ids[at + 1], "building-3d");
        let style = parse_and_compile(serde_json::to_vec(&doc).unwrap().as_slice()).unwrap();
        let StyleLayer::Extrusion { height, base, min_zoom, opacity, .. } = style.layers.iter().find(|l| l.id() == "building-3d").unwrap() else { panic!() };
        assert_eq!(*height, crate::style::ExtrusionValue::Get("height".into()));
        assert_eq!(*base, crate::style::ExtrusionValue::Get("min_height".into()));
        assert_eq!(*min_zoom, Some(15));
        assert_eq!(opacity.value_at(16), 0.6);
    }

    #[test]
    fn with_3d_buildings_refuses_twice_and_refuses_a_style_without_buildings() {
        let mut doc: serde_json::Value = serde_json::from_slice(&mapbox_fixture_or_skip!("mapbox-streets-style.json")).unwrap();
        with_3d_buildings(&mut doc).unwrap();
        assert!(with_3d_buildings(&mut doc).is_err());
        let mut bare = serde_json::json!({"version": 8, "sources": {}, "layers": []});
        assert!(with_3d_buildings(&mut bare).is_err());
    }

    #[test]
    fn the_vendored_streets_style_is_untouched() {
        let style = or_skip!(mapbox_streets());
        assert!(!style.layers.iter().any(|l| matches!(l, StyleLayer::Extrusion { .. })), "R14: opt-in only");
    }

    /// Mapbox Streets is filtered on its own `extrude` flag, never on
    /// OpenMapTiles' `hide_3d`, which Mapbox tiles do not carry.
    #[test]
    fn with_3d_buildings_on_streets_filters_on_extrude() {
        let mut doc: serde_json::Value = serde_json::from_slice(&mapbox_fixture_or_skip!("mapbox-streets-style.json")).unwrap();
        with_3d_buildings(&mut doc).unwrap();
        let layer = doc["layers"].as_array().unwrap().iter().find(|l| l["id"] == "building-3d").unwrap();
        assert_eq!(layer["source"], "composite", "the source is taken from `building`");
        assert_eq!(layer["filter"], serde_json::json!(["==", ["get", "extrude"], "true"]));
        assert_eq!(layer["paint"]["fill-extrusion-color"], "hsl(20, 15%, 85%)", "Streets' own building fill-color");
    }

    /// Ruling (Task 7 fix round 1, amends R19): an OpenMapTiles `building`
    /// layer gets `render_height`/`render_min_height` and the `hide_3d`
    /// clause, so an outline whose `building:part` pieces are mapped
    /// separately does not extrude as a whole-lot slab over them.
    #[test]
    fn with_3d_buildings_on_an_openmaptiles_source_filters_hide_3d() {
        let mut doc: serde_json::Value = serde_json::from_slice(&std::fs::read("examples/liberty-style.json").unwrap()).unwrap();
        // Liberty ships its own `building-3d`; take it out to opt back in.
        doc["layers"].as_array_mut().unwrap().retain(|l| l["id"] != "building-3d");
        with_3d_buildings(&mut doc).expect("Liberty has a building layer");
        let layers = doc["layers"].as_array().unwrap();
        let at = layers.iter().position(|l| l["id"] == "building").unwrap();
        let layer = &layers[at + 1];
        assert_eq!(layer["id"], "building-3d");
        assert_eq!(layer["source"], "openmaptiles");
        assert_eq!(layer["filter"], serde_json::json!(["!=", ["get", "hide_3d"], true]));
        let style = parse_and_compile(serde_json::to_vec(&doc).unwrap().as_slice()).unwrap();
        let StyleLayer::Extrusion { height, base, .. } = style.layers.iter().find(|l| l.id() == "building-3d").unwrap() else { panic!() };
        assert_eq!(*height, crate::style::ExtrusionValue::Get("render_height".into()));
        assert_eq!(*base, crate::style::ExtrusionValue::Get("render_min_height".into()));
    }
}

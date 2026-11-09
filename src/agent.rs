//! Agent-callable functions over a [`MapView`]: what this crate can be
//! asked to do, declared in a form an agent framework can read, and a
//! dispatcher that routes a name and a JSON payload to the real method.
//!
//! # Why this crate owns both halves
//!
//! [`agent_functions`] declares; [`agent_dispatch`] calls. A consumer's
//! registry aggregates declarations from every component it hosts for
//! discovery, and routes anything named `map.*` straight into
//! [`agent_dispatch`]. It deliberately does NOT hand-write the calling
//! glue on its side, because glue written there would drift from the
//! signatures here with nothing to catch it -- a renamed parameter would
//! surface as a runtime `Err` months later.
//!
//! Owning both halves moves that failure to compile-and-test time in the
//! crate that caused it. `every_declared_function_is_dispatchable`
//! and `every_dispatchable_name_is_declared` are the guard: adding a
//! declaration without an arm, or an arm without a declaration, fails the
//! build here rather than a call over there.
//!
//! # Shape of a declaration
//!
//! MCP-tool-shaped -- `name`, `description`, a JSON-Schema `params` --
//! plus two fields MCP does not standardise:
//!
//! - `returns`, a JSON Schema for the success value, so a caller knows
//!   what it is getting without calling first.
//! - `safety`, required, one of `read`/`write`/`destructive`. The consumer
//!   surfaces this to an agent BEFORE it calls, so it is marked honestly:
//!   `destructive` is for a call this interface cannot undo (none of the
//!   current functions is one; the webview backend's `map.set_base_style`
//!   was, and left with it).
//!
//! # Instance addressing is the caller's problem
//!
//! [`agent_dispatch`] takes the `&mut MapView` it is given. There is no
//! handle, no registry of instances, no "which map" argument -- a host
//! with several maps resolves which one before calling, which is a
//! question about the host's own object graph and not one this crate can
//! answer.

use serde_json::{json, Map, Value};

use crate::map_view::MapView;

/// The version of this crate the declarations came from, so a registry can
/// record what it loaded and notice when it changes.
pub const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The prefix every function in this crate's namespace carries. A host
/// registry routes on it.
pub const NAMESPACE: &str = "map.";

/// What calling a function can cost, worst case.
///
/// Surfaced to the agent before it calls, so the distinctions are about
/// consequence, not implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Safety {
    /// Observes. Changes nothing an later call could notice.
    Read,
    /// Changes what the map shows, reversibly: another call with different
    /// arguments puts it back.
    Write,
    /// Can leave the map in a state this interface cannot undo.
    Destructive,
}

/// One agent-callable function.
///
/// `params` and `returns` are JSON Schema documents (draft 2020-12 shapes,
/// though nothing here validates against a meta-schema). `returns`
/// describes the `Ok` value only -- an error is a plain string, always.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FunctionDecl {
    pub name: String,
    pub description: String,
    pub params: Value,
    pub returns: Value,
    pub safety: Safety,
    /// The `gpui-maps` version these declarations came from. Carried on
    /// every declaration rather than once beside the list so that a
    /// registry that stores declarations individually -- most do -- keeps
    /// the provenance attached to each one.
    pub crate_version: String,
}

/// Every function this crate offers an agent.
///
/// Ordered read-before-write, and stable: a host may render this list
/// directly.
///
/// `map.capture` (headless render to PNG) is deliberately absent. It was
/// deferred by agreement with the consumer until the allocation counters
/// this ships alongside have done their job.
#[must_use]
pub fn agent_functions() -> Vec<FunctionDecl> {
    let lon_lat_zoom = json!({
        "type": "object",
        "properties": {
            "lon": { "type": "number", "minimum": -180, "maximum": 180 },
            "lat": { "type": "number", "minimum": -90, "maximum": 90 },
            "zoom": { "type": "number", "minimum": 0, "maximum": 20 },
        },
        "required": ["lon", "lat", "zoom"],
        "additionalProperties": false,
    });
    let empty = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let ok = json!({
        "type": "object",
        "properties": { "ok": { "type": "boolean" } },
        "required": ["ok"],
    });

    vec![
        decl(
            "map.get_camera",
            "The camera's current centre, zoom, pitch and bearing, and the viewport it \
             is looking through. Reads back mid-ease: `map.fly_to` animates, so the \
             values here will still be moving after it returns.",
            empty.clone(),
            json!({
                "type": "object",
                "properties": {
                    "lon": { "type": "number" },
                    "lat": { "type": "number" },
                    "zoom": { "type": "number" },
                    "pitch_radians": { "type": "number", "description": "0 = looking straight down" },
                    "bearing_radians": { "type": "number" },
                    "viewport_width_px": { "type": "number" },
                    "viewport_height_px": { "type": "number" },
                },
                "required": ["lon", "lat", "zoom"],
            }),
            Safety::Read,
        ),
        decl(
            "map.list_layers",
            "The style layers this map actually compiled, in draw order, alongside the \
             layers the style document declared that were dropped and why. A layer is \
             absent from `layers` and present in `skipped` when it used a construct this \
             renderer does not implement; it is in `hidden` when the style author (or a \
             config key they gated it on) switched it off, which is not a gap in the \
             renderer. `skipped`/`hidden` describe the most recent style compile in this \
             process, which for a single-map host is this map's.",
            empty.clone(),
            json!({
                "type": "object",
                "properties": {
                    "layers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "kind": { "enum": ["background", "fill", "line", "symbol"] },
                                "min_zoom": { "type": ["integer", "null"] },
                                "max_zoom": { "type": ["integer", "null"] },
                            },
                            "required": ["id", "kind"],
                        },
                    },
                    "skipped": { "type": "array", "items": diagnostic_schema() },
                    "hidden": { "type": "array", "items": diagnostic_schema() },
                },
                "required": ["layers", "skipped", "hidden"],
            }),
            Safety::Read,
        ),
        decl(
            "map.get_diagnostics",
            "Everything the most recent style compile had to do to the style document \
             that the document did not ask for: layers dropped, layers hidden, and \
             properties degraded to a literal because the constant folder could not \
             reduce them. A degraded property means the layer still draws, with one \
             value replaced -- which is how a colour ends up wrong rather than a whole \
             road class ending up missing.",
            empty.clone(),
            json!({
                "type": "object",
                "properties": {
                    "diagnostics": { "type": "array", "items": diagnostic_schema() },
                    "counts": {
                        "type": "object",
                        "properties": {
                            "skipped-layer": { "type": "integer" },
                            "hidden-layer": { "type": "integer" },
                            "degraded-property": { "type": "integer" },
                        },
                    },
                },
                "required": ["diagnostics", "counts"],
            }),
            Safety::Read,
        ),
        decl(
            "map.query_features_at",
            "The overlay feature under a point in the map's own viewport pixel space, if \
             any. Answered from what is ON SCREEN, not from what was most recently \
             pushed: during the window where a `map.set_features` bake is still in \
             flight this reports the features the user can actually see. Basemap \
             features are not queryable through this.",
            json!({
                "type": "object",
                "properties": {
                    "x": { "type": "number", "description": "logical pixels from the map's left edge" },
                    "y": { "type": "number", "description": "logical pixels from the map's top edge" },
                },
                "required": ["x", "y"],
                "additionalProperties": false,
            }),
            json!({
                "type": "object",
                "properties": {
                    "feature": {
                        "type": ["object", "null"],
                        "properties": {
                            "index": { "type": "integer" },
                            "id": { "type": ["string", "number", "null"] },
                        },
                        "required": ["index"],
                    },
                },
                "required": ["feature"],
            }),
            Safety::Read,
        ),
        decl(
            "map.get_stats",
            "GPU buffer allocation counters, plus process heap-in-use and RSS. \
             `gpu_buffers_created_total` climbing while `gpu_buffer_bytes_live` stays \
             flat means buffers are being recreated rather than reused. \
             `gpu_buffer_bytes_live` counts only what this crate allocated and only what \
             it has not dropped; a gap against `rss_bytes` is expected and is not by \
             itself evidence of a leak here. Read `heap_in_use_bytes` (what the allocator \
             says is live) next to `rss_bytes` (what the kernel says is resident, \
             including pages the allocator freed but kept): heap flat while RSS climbs is \
             page retention, not a leak.",
            empty.clone(),
            json!({
                "type": "object",
                "properties": {
                    "gpu_buffers_created_total": { "type": "integer" },
                    "gpu_buffer_bytes_created_total": { "type": "integer" },
                    "gpu_buffers_live": { "type": "integer" },
                    "gpu_buffer_bytes_live": { "type": "integer" },
                    "mesh_installs_total": { "type": "integer" },
                    "overlay_generation": { "type": "integer" },
                    "tiles_resident": { "type": "integer" },
                    "rss_bytes": { "type": "integer" },
                    "heap_in_use_bytes": { "type": "integer" },
                },
                "required": [
                    "gpu_buffers_created_total",
                    "gpu_buffer_bytes_created_total",
                    "gpu_buffers_live",
                    "gpu_buffer_bytes_live",
                    "mesh_installs_total",
                    "overlay_generation",
                    "tiles_resident",
                    "rss_bytes",
                    "heap_in_use_bytes",
                ],
            }),
            Safety::Read,
        ),
        decl(
            "map.get_space",
            "Everything the globe is sitting in and lit by: the space colour, the star \
             field, and the atmosphere's intensity, night floor and sun. The read half \
             of `map.set_space`, `map.set_atmosphere`, `map.set_star_intensity` and \
             `map.set_terminator` -- so a caller can record a term, change it, look, and \
             put back exactly what was there. \
             \
             `globe_transition` travels with it because NOTHING here reaches a pixel at \
             0.0: every term is gated on the space transition, which is 1.0 at zoom 3 \
             and below, exactly 0.0 from zoom 6 up, and 0.0 at every zoom while \
             `map.set_globe` has the globe off. A write that moves \
             a value while the screen does not move is the CORRECT outcome above zoom 6, \
             not a broken setter, and this field is the only way to tell that apart from \
             a setter that did nothing.",
            empty.clone(),
            space_schema(),
            Safety::Read,
        ),
        decl(
            "map.set_camera",
            "Puts the camera at a centre and zoom immediately, with no animation. Zoom \
             is clamped to 0..=20. Reversible: call it again with the previous values, \
             which `map.get_camera` will give you.",
            lon_lat_zoom.clone(),
            ok.clone(),
            Safety::Write,
        ),
        decl(
            "map.fly_to",
            "A documented no-op, kept so older agents do not break: it animated the \
             camera on the removed webview backend. Use `map.set_camera`.",
            lon_lat_zoom,
            ok.clone(),
            Safety::Write,
        ),
        decl(
            "map.set_features",
            "Replaces the whole overlay feature collection with a GeoJSON \
             FeatureCollection. WHOLE-STATE: what is passed is what is on the map \
             afterwards, and an empty collection clears it. A rejected call changes \
             nothing at all. The new geometry is tessellated off-thread and reaches the \
             screen on a later frame, so this returning does not mean it is drawn yet.",
            json!({
                "type": "object",
                "properties": {
                    "geojson": {
                        "description": "A GeoJSON FeatureCollection, either as an object or as a JSON string.",
                        "type": ["object", "string"],
                    },
                },
                "required": ["geojson"],
                "additionalProperties": false,
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "feature_count": { "type": "integer" },
                    "generation": { "type": "integer" },
                },
                "required": ["ok", "feature_count", "generation"],
            }),
            Safety::Write,
        ),
        decl(
            "map.set_feature_paint",
            "Restyles named overlay features in place, without replacing the collection. \
             Every update is resolved before any is applied, so an unknown id leaves the \
             map exactly as it was -- including the valid updates beside it. Paint \
             properties are the GeoJSON `properties` this crate reads (`line-color`, \
             `line-width`, `fill-color`, `circle-radius`, `icon-image`, ...).",
            json!({
                "type": "object",
                "properties": {
                    "updates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": ["string", "number"],
                                    "description": "The feature's GeoJSON `id`. Features without one cannot be addressed.",
                                },
                                "paint": { "type": "object" },
                            },
                            "required": ["id", "paint"],
                        },
                    },
                },
                "required": ["updates"],
                "additionalProperties": false,
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "updated": { "type": "integer" },
                },
                "required": ["ok", "updated"],
            }),
            Safety::Write,
        ),
        decl(
            "map.set_globe",
            "Switches the globe projection on or off for every zoom. ON (the default) \
             is this renderer's normal behaviour: the projection is spherical up to \
             zoom 11, blends to flat Mercator by zoom 12, and is flat above it, with \
             the backdrop, atmosphere and starfield fading out on their own range \
             (zoom 3 to 6). OFF pins the projection to pure Mercator at every zoom and \
             takes those space effects with it, because they exist to dress a curved \
             limb that is no longer on screen. Nothing else changes -- not the camera, \
             not the terminator's own setting, not the brand mark -- so this is fully \
             reversible by calling it again with the other value, and the returned \
             `globe_enabled` is the state that is now in force. Use it to A/B a \
             suspected projection artefact against the flat frame.",
            json!({
                "type": "object",
                "properties": {
                    "enabled": {
                        "description": "true for the globe (the default), false for flat Mercator at every zoom.",
                        "type": "boolean",
                    },
                },
                "required": ["enabled"],
                "additionalProperties": false,
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "globe_enabled": { "type": "boolean" },
                    "projection_transition": {
                        "description":
                            "The projection's globeness at the camera's CURRENT zoom after \
                             this call: 1.0 fully spherical, 0.0 pure Mercator. Always 0.0 \
                             when the globe is off.",
                        "type": "number",
                    },
                },
                "required": ["ok", "globe_enabled", "projection_transition"],
            }),
            Safety::Write,
        ),
        decl(
            "map.set_atmosphere",
            "Changes the limb glow, the night floor, or where the light comes from. \
             \
             SETTABLE IN ISOLATION, and that is the whole point of the shape: every \
             field is optional and an omitted one KEEPS ITS CURRENT VALUE. Send \
             `{\"intensity\": 0}` and the sun and the night floor are exactly where they \
             were. This is what makes the one diagnostic that matters possible -- change \
             one term, look at the screen, change it back -- which a whole-struct setter \
             destroys, because a caller who had to supply the other fields could not \
             know what else they had just moved. \
             \
             `intensity` multiplies the whole glow, 0.0 being off. `night_brightness` \
             multiplies the basemap's fills on the night side: 0.0 is a black night, \
             1.0 is no ground shading at all -- which is the terminator switched OFF, \
             not a bright night side. Prefer `map.set_terminator` for that flip; this is \
             the knob for the value in between. `sun` moves the light. \
             \
             Gated on the globe like everything in `map.get_space`: above zoom 6 this \
             changes state and no pixels. Returns the resulting atmosphere in full, so \
             the response is the value to send back to undo it.",
            atmosphere_patch_schema(),
            space_schema(),
            Safety::Write,
        ),
        decl(
            "map.set_star_intensity",
            "Multiplies every star's alpha. `0.0` is OFF, not dim -- the star draw and \
             its 1.24 MB of buffers are never created until a non-zero intensity is \
             actually drawn, and `0.0` is the default, so a map that has never been told \
             otherwise has no star field at all. Clamped to 0..=1 before it reaches the \
             shader. \
             \
             A one-field door onto the same state `map.set_space` writes wholesale, \
             mirroring `MapView::set_star_intensity`: it is the field a caller actually \
             reaches for, and reaching for it should not mean naming the space colour \
             too.",
            json!({
                "type": "object",
                "properties": {
                    "intensity": {
                        "type": "number",
                        "description": "0.0 is off (no star draw at all); 1.0 is the catalogue's own brightness.",
                    },
                },
                "required": ["intensity"],
                "additionalProperties": false,
            }),
            space_schema(),
            Safety::Write,
        ),
        decl(
            "map.set_terminator",
            "Switches the day/night terminator on or off -- the same flip the demo's `T` \
             key performs, reachable without a keystroke, which is the point: a \
             diagnosis that needed this used to require a second window, and a second \
             window steals OS focus from the one the human is watching. \
             \
             `on: false` sets `night_brightness` to exactly 1.0, the value that means \
             \"no ground shading\" rather than a large number that happens to clamp \
             there. `on: true` uses your `night_brightness` if you give one, keeps the \
             floor already in place if the terminator was on (so calling it twice \
             changes nothing), and otherwise falls back to the terminator's own default \
             floor of 0.25 (the engine starts with the terminator OFF, at 1.0). \
             \
             That last case is the ONE thing the `T` key can do that this cannot: \
             switching off overwrites the floor with 1.0, and this function has no \
             memory of what was there before. The demo's key does, because it seeds \
             itself from `GPUI_MAP_NIGHT` at startup. If your floor is not 0.25, read \
             `map.get_space` before switching off and pass the value back when you \
             switch on. \
             \
             `intensity` and the sun are untouched either way.",
            json!({
                "type": "object",
                "properties": {
                    "on": {
                        "type": "boolean",
                        "description": "true dims the night side, false leaves it at full basemap brightness.",
                    },
                    "night_brightness": {
                        "type": "number",
                        "description": "The floor to switch ON with. Ignored when `on` is false, which is always exactly 1.0. Optional; defaults to the floor already in force, else 0.25.",
                    },
                },
                "required": ["on"],
                "additionalProperties": false,
            }),
            space_schema(),
            Safety::Write,
        ),
        decl(
            "map.set_space",
            "Everything `map.set_atmosphere` and `map.set_star_intensity` write, plus \
             the two terms neither of them reaches: the colour outside the planet's \
             silhouette and the star size scale. The whole-struct door, matching \
             `MapView::set_space`. \
             \
             PARTIAL, on the same rule as `map.set_atmosphere`: every field is optional \
             and an omitted one keeps its current value, including inside the nested \
             `atmosphere` object. There is no way through this interface to supply a \
             whole `Space` and clobber a field you did not mean to name. \
             \
             `color` is where the globe sits -- pure black by default, measured against \
             the darkest style this repo ships rather than picked. `star_size_scale` \
             multiplies every star's radius; 1.0 puts the faintest catalogue star at 1.2 \
             logical pixels and Sirius at 4.91. Both, like everything else here, are \
             gated on the globe transition.",
            json!({
                "type": "object",
                "properties": {
                    "color": {
                        "type": "string",
                        "description": "`#rgb`, `#rrggbb` or `#rrggbbaa`. Named CSS colours are not accepted.",
                    },
                    "star_intensity": { "type": "number" },
                    "star_size_scale": { "type": "number" },
                    "atmosphere": atmosphere_patch_schema(),
                },
                "additionalProperties": false,
            }),
            space_schema(),
            Safety::Write,
        ),
    ]
}

/// Calls one of [`agent_functions`]'s functions on `target`.
///
/// `name` is the fully-qualified name including the `map.` prefix, exactly
/// as declared -- a host that strips its own routing prefix must put it
/// back. An unknown name is an `Err`, never a silent no-op.
///
/// Errors are plain strings meant to be shown to an agent: what was wrong
/// with the call, in words, not a type. A failed call leaves the map
/// unchanged.
///
/// # What the host still owes on a write
///
/// `map.set_features` and `map.set_feature_paint` reach the store here but
/// their off-thread bake is spawned from gpui's render pass, which needs a
/// `gpui::Context` this signature deliberately does not take (see the
/// module doc comment). **A host must call `cx.notify()` on the map entity
/// after a successful write**, from inside the same `entity.update` this
/// was called in. Without that, gpui never re-renders, the bake is never
/// spawned, and the new features sit in the store unseen.
///
/// # Errors
///
/// Returns `Err` for an unknown function name, arguments that do not match
/// the declared `params` schema, and anything the underlying method
/// rejects (malformed GeoJSON, an unknown feature id).
pub fn agent_dispatch(target: &mut MapView, name: &str, args: Value) -> Result<Value, String> {
    match name {
        "map.get_camera" => {
            let camera = target.camera();
            Ok(json!({
                "lon": camera.center_lon,
                "lat": camera.center_lat,
                "zoom": camera.zoom,
                "pitch_radians": camera.pitch,
                "bearing_radians": camera.bearing,
                "viewport_width_px": camera.viewport_width_px,
                "viewport_height_px": camera.viewport_height_px,
            }))
        }

        "map.list_layers" => {
            let layers: Vec<Value> = target
                .style()
                .layers
                .iter()
                .map(|layer| {
                    let (min_zoom, max_zoom) = layer.zoom_range();
                    json!({
                        "id": layer.id(),
                        "kind": layer.kind(),
                        "min_zoom": min_zoom,
                        "max_zoom": max_zoom,
                    })
                })
                .collect();
            let diagnostics = crate::style_spec::compile_diagnostics();
            let of_kind = |wanted: crate::style_spec::DiagnosticKind| -> Vec<Value> {
                diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.kind == wanted)
                    .map(|diagnostic| serde_json::to_value(diagnostic).unwrap_or(Value::Null))
                    .collect()
            };
            Ok(json!({
                "layers": layers,
                "skipped": of_kind(crate::style_spec::DiagnosticKind::SkippedLayer),
                "hidden": of_kind(crate::style_spec::DiagnosticKind::HiddenLayer),
            }))
        }

        "map.get_diagnostics" => {
            let diagnostics = crate::style_spec::compile_diagnostics();
            let count = |wanted: crate::style_spec::DiagnosticKind| {
                diagnostics.iter().filter(|diagnostic| diagnostic.kind == wanted).count()
            };
            Ok(json!({
                "diagnostics": diagnostics,
                "counts": {
                    "skipped-layer": count(crate::style_spec::DiagnosticKind::SkippedLayer),
                    "hidden-layer": count(crate::style_spec::DiagnosticKind::HiddenLayer),
                    "degraded-property": count(crate::style_spec::DiagnosticKind::DegradedProperty),
                },
            }))
        }

        "map.query_features_at" => {
            let object = as_object(&args)?;
            let x = number(object, "x")? as f32;
            let y = number(object, "y")? as f32;
            let feature = target.feature_at(x, y).map(|found| {
                json!({
                    "index": found.index,
                    "id": found.id.as_ref().map(feature_id_to_json),
                })
            });
            Ok(json!({ "feature": feature }))
        }

        "map.get_stats" => serde_json::to_value(crate::render::allocation_stats())
            .map_err(|error| format!("could not serialise the allocation counters: {error}")),

        "map.get_space" => Ok(space_json(target)),

        "map.set_camera" => {
            let (lon, lat, zoom) = lon_lat_zoom(&args)?;
            target.set_camera(lon, lat, zoom);
            Ok(json!({ "ok": true }))
        }

        "map.fly_to" => {
            let (lon, lat, zoom) = lon_lat_zoom(&args)?;
            // A documented no-op: the webview backend that animated this
            // was removed. Still declared, and still validating its
            // arguments, so an agent built against the old surface gets
            // `ok` rather than "unknown function".
            let _ = (lon, lat, zoom);
            Ok(json!({ "ok": true }))
        }

        "map.set_features" => {
            let object = as_object(&args)?;
            let geojson = match object.get("geojson") {
                Some(Value::String(text)) => text.clone(),
                Some(value @ Value::Object(_)) => value.to_string(),
                Some(_) => {
                    return Err("\"geojson\" must be a FeatureCollection object or a JSON string"
                        .to_owned())
                }
                None => return Err("missing required argument \"geojson\"".to_owned()),
            };
            target.set_features_deferred(&geojson).map_err(|error| error.to_string())?;
            Ok(json!({
                "ok": true,
                "feature_count": target.overlay().features().len(),
                "generation": target.overlay().generation(),
            }))
        }

        "map.set_feature_paint" => {
            let object = as_object(&args)?;
            let updates = object
                .get("updates")
                .and_then(Value::as_array)
                .ok_or_else(|| "missing required argument \"updates\" (an array)".to_owned())?;
            let mut parsed = Vec::with_capacity(updates.len());
            for (index, update) in updates.iter().enumerate() {
                let update = update
                    .as_object()
                    .ok_or_else(|| format!("updates[{index}] must be an object"))?;
                let id = update
                    .get("id")
                    .ok_or_else(|| format!("updates[{index}] has no \"id\""))?;
                let id = json_to_feature_id(id)
                    .ok_or_else(|| format!("updates[{index}].id must be a string or a number"))?;
                let paint = update
                    .get("paint")
                    .and_then(Value::as_object)
                    .ok_or_else(|| format!("updates[{index}] has no \"paint\" object"))?;
                let paint = crate::overlay::FeaturePaint::from_properties(paint)
                    .map_err(|error| format!("updates[{index}].paint: {error}"))?;
                parsed.push((id, paint));
            }
            target.set_feature_paint_deferred(&parsed).map_err(|error| error.to_string())?;
            Ok(json!({ "ok": true, "updated": parsed.len() }))
        }

        "map.set_atmosphere" => {
            let object = as_object(&args)?;
            // Resolved in full BEFORE anything is written, so a bad `sun`
            // beside a good `intensity` leaves the map exactly as it was.
            // `safety: write` is a promise that a failed write did not
            // half-happen, and a half-applied patch is the one failure a
            // caller doing "change one thing and change it back" could not
            // recover from -- they would not know which half landed.
            let patched = atmosphere_patch(target.space().atmosphere, object)?;
            target.set_atmosphere(patched);
            Ok(space_json(target))
        }

        "map.set_star_intensity" => {
            let object = as_object(&args)?;
            reject_unknown_keys(object, &["intensity"])?;
            let intensity = number(object, "intensity")? as f32;
            target.set_star_intensity(intensity);
            Ok(space_json(target))
        }

        "map.set_terminator" => {
            let object = as_object(&args)?;
            reject_unknown_keys(object, &["on", "night_brightness"])?;
            let on = object
                .get("on")
                .ok_or_else(|| "missing required argument \"on\" (a boolean)".to_owned())?
                .as_bool()
                .ok_or_else(|| "argument \"on\" must be a boolean".to_owned())?;
            let atmosphere = target.space().atmosphere;
            let named = optional_number(object, "night_brightness")?;
            // The same three branches `Terminator::next` has in the demo,
            // in the same order, plus the caller's own value ahead of them.
            // The third one is the reason this is not simply
            // `TERMINATOR_NIGHT_BRIGHTNESS`: a
            // terminator that is ALREADY on has a floor, and resetting it
            // to the default would be this function quietly moving a term
            // the caller did not name -- the one thing every function here
            // promises not to do. So `{"on": true}` twice is idempotent.
            let night_brightness = match (on, named) {
                (false, _) => 1.0,
                (true, Some(value)) => value as f32,
                (true, None) if atmosphere.night_brightness_clamped() < 1.0 => {
                    atmosphere.night_brightness
                }
                // NOT `Atmosphere::default()`: that is 1.0 since the
                // 2026-09-29 ruling, the terminator OFF, and `on: true`
                // must actually dim the night side.
                (true, None) => crate::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            };
            target.set_atmosphere(crate::render::atmosphere::Atmosphere {
                night_brightness,
                ..atmosphere
            });
            Ok(space_json(target))
        }

        "map.set_space" => {
            let object = as_object(&args)?;
            reject_unknown_keys(
                object,
                &["color", "star_intensity", "star_size_scale", "atmosphere"],
            )?;
            // Built from the CURRENT space, so every field this call does
            // not name survives it -- the same rule the nested atmosphere
            // patch follows, applied to the outer struct.
            let mut space = target.space();
            if let Some(color) = object.get("color") {
                let text = color
                    .as_str()
                    .ok_or_else(|| "argument \"color\" must be a string".to_owned())?;
                space.color = crate::overlay::paint::parse_color(text).ok_or_else(|| {
                    format!(
                        "argument \"color\" must be #rgb, #rrggbb or #rrggbbaa, got {text:?} \
                         -- named CSS colours are not accepted"
                    )
                })?;
            }
            if let Some(intensity) = optional_number(object, "star_intensity")? {
                space.star_intensity = intensity as f32;
            }
            if let Some(scale) = optional_number(object, "star_size_scale")? {
                space.star_size_scale = scale as f32;
            }
            if let Some(atmosphere) = object.get("atmosphere") {
                let atmosphere = atmosphere.as_object().ok_or_else(|| {
                    "argument \"atmosphere\" must be an object".to_owned()
                })?;
                space.atmosphere = atmosphere_patch(space.atmosphere, atmosphere)?;
            }
            target.set_space(space);
            Ok(space_json(target))
        }

        "map.set_globe" => {
            let object = as_object(&args)?;
            let enabled = match object.get("enabled") {
                Some(Value::Bool(enabled)) => *enabled,
                Some(_) => return Err("\"enabled\" must be true or false".to_owned()),
                None => return Err("missing required argument \"enabled\"".to_owned()),
            };
            target.set_globe_enabled(enabled);
            // The transition is reported back rather than left to be
            // inferred, because "globe on" and "this frame is curved" are
            // different claims: a caller at z13 who turns the globe ON gets
            // `globe_enabled: true` and `projection_transition: 0.0`, and
            // that pair is the answer to "why did nothing change on
            // screen?" without a second round trip.
            let transition = if enabled {
                crate::render::globe::transition_for_zoom(target.camera().zoom)
            } else {
                0.0
            };
            Ok(json!({
                "ok": true,
                "globe_enabled": target.globe_enabled(),
                "projection_transition": transition,
            }))
        }

        other => Err(format!(
            "unknown function {other:?} -- call map.list_layers's sibling \
             `agent_functions()` for what this crate offers"
        )),
    }
}

fn decl(
    name: &str,
    description: &str,
    params: Value,
    returns: Value,
    safety: Safety,
) -> FunctionDecl {
    FunctionDecl {
        name: name.to_owned(),
        description: description.to_owned(),
        params,
        returns,
        safety,
        crate_version: CRATE_VERSION.to_owned(),
    }
}

/// The JSON Schema for one `style_spec::StyleDiagnostic`, written once so
/// `map.list_layers` and `map.get_diagnostics` cannot come to describe the
/// same struct differently.
fn diagnostic_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "enum": ["skipped-layer", "hidden-layer", "degraded-property"] },
            "layer": { "type": ["string", "null"], "description": "The style layer's id, where the notice names one." },
            "detail": { "type": "string" },
        },
        "required": ["kind", "detail"],
    })
}

/// The JSON Schema for a PARTIAL `Atmosphere`, written once because
/// `map.set_atmosphere` takes it at the top level and `map.set_space` takes
/// the same object nested -- and the two describing the same patch
/// differently is exactly the drift the module doc comment is about.
///
/// Nothing is `required`. That is not laxity, it is the contract: an omitted
/// field keeps its current value, so a caller changing one term does not
/// have to state -- or even know -- the other two.
fn atmosphere_patch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "intensity": {
                "type": "number",
                "description": "Multiplies the limb glow. 0.0 is off, 1.0 is the default. Clamped to 0..=1 at the shader.",
            },
            "night_brightness": {
                "type": "number",
                "description": "How bright the night side's fills are. 0.0 is black, 0.25 the terminator's own floor, 1.0 (the default) the terminator off.",
            },
            "sun": sun_schema(),
        },
        "additionalProperties": false,
    })
}

/// The JSON Schema for `atmosphere::Sun`, as a tagged union on `mode`.
///
/// Tagged rather than shape-inferred (`{"lon_deg": ...}` implying `map`)
/// because two of the four variants carry a pair of degrees and an agent
/// that guessed wrong would get a plausible-looking light in the wrong
/// coordinate system -- a wrong answer that renders, which is the failure
/// mode this whole surface exists to avoid.
fn sun_schema() -> Value {
    json!({
        "type": "object",
        "description": "Where the light comes from. `now` (the default) is the real subsolar point \
                        read from the wall clock every frame; `utc` is the same arithmetic on an \
                        instant you name; `map` is a subsolar point chosen directly, which stays \
                        put on the planet as the camera moves; `viewport` fixes the light to the \
                        SCREEN, for a caller who wants to aim it rather than be truthful.",
        "properties": {
            "mode": { "enum": ["now", "utc", "map", "viewport"] },
            "unix_seconds": { "type": "number", "description": "mode=utc: seconds since the Unix epoch." },
            "lon_deg": { "type": "number", "description": "mode=map: the subsolar longitude." },
            "lat_deg": { "type": "number", "description": "mode=map: the subsolar latitude." },
            "tilt_deg": {
                "type": "number",
                "description": "mode=viewport: 0 is a sun directly behind the viewer (no terminator), 90 puts the terminator through the middle of the disc, 180 is back-lit.",
            },
            "bearing_deg": {
                "type": "number",
                "description": "mode=viewport: where around the disc the lit side sits, clockwise from straight up.",
            },
        },
        "required": ["mode"],
        "additionalProperties": false,
    })
}

/// The JSON Schema every space function returns, reads and writes alike.
///
/// One schema for all five because a write here answers with the state it
/// produced rather than a bare `{"ok": true}`: the response to a write IS
/// the argument that undoes it, and that only holds if the two are the same
/// shape.
fn space_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "color": { "type": "string", "description": "`#rrggbbaa`." },
            "star_intensity": { "type": "number" },
            "star_alpha": { "type": "number", "description": "`star_intensity` clamped to 0..=1 -- what the shader is handed." },
            "star_size_scale": { "type": "number" },
            "atmosphere": {
                "type": "object",
                "properties": {
                    "intensity": { "type": "number" },
                    "night_brightness": { "type": "number" },
                    "night_brightness_clamped": {
                        "type": "number",
                        "description": "`night_brightness` clamped to 0..=1 -- what the shader is handed.",
                    },
                    "terminator": {
                        "type": "boolean",
                        "description": "Whether the night side is dimmed at all, i.e. `night_brightness_clamped` < 1.0.",
                    },
                    "sun": sun_schema(),
                },
                "required": [
                    "intensity",
                    "night_brightness",
                    "night_brightness_clamped",
                    "terminator",
                    "sun",
                ],
            },
            "globe_transition": {
                "type": "number",
                "description": "1.0 at zoom 3 and below, 0.0 from zoom 6 up, and 0.0 at every zoom while the globe is off. At 0.0 nothing in this object reaches a pixel.",
            },
        },
        "required": ["color", "star_intensity", "star_size_scale", "atmosphere", "globe_transition"],
    })
}

fn as_object(args: &Value) -> Result<&Map<String, Value>, String> {
    args.as_object().ok_or_else(|| {
        format!("arguments must be a JSON object, got {}", type_name_of(args))
    })
}

fn type_name_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn number(object: &Map<String, Value>, key: &str) -> Result<f64, String> {
    object
        .get(key)
        .ok_or_else(|| format!("missing required argument {key:?}"))?
        .as_f64()
        .ok_or_else(|| format!("argument {key:?} must be a number"))
}

/// A number that may be absent, but must be a number if it is there.
///
/// The distinction [`number`] cannot make, and the one every partial write
/// here depends on: absent means "keep what is there", whereas a misspelled
/// key that silently read as absent would report success having changed
/// nothing. [`reject_unknown_keys`] is the other half of that guard.
fn optional_number(object: &Map<String, Value>, key: &str) -> Result<Option<f64>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("argument {key:?} must be a number")),
    }
}

/// Refuses a key the function does not know.
///
/// The declarations say `additionalProperties: false`, but nothing between
/// an agent and [`agent_dispatch`] is obliged to validate against them. On a
/// function where EVERY field is optional, an unvalidated typo is the worst
/// possible outcome: `{"intesity": 0}` would change nothing, return the
/// state unchanged, and read as "the setter is broken" or, worse, as "the
/// atmosphere was not what was causing it". Both are wrong conclusions drawn
/// from a successful call, which is precisely what this surface exists to
/// prevent.
fn reject_unknown_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "unknown argument {key:?} -- this function takes {allowed:?}. Every one \
                 of them is optional and an omitted one keeps its current value, so a \
                 misspelled name would otherwise have looked like a successful no-op."
            ));
        }
    }
    Ok(())
}

/// `current` with the fields `object` names replaced, and the rest kept.
///
/// The load-bearing helper behind `map.set_atmosphere` and the nested
/// `atmosphere` of `map.set_space`: it is where "every term is settable in
/// isolation" actually happens. Fails before it returns anything if any
/// field is malformed, so the caller either gets the whole patch or none of
/// it.
fn atmosphere_patch(
    current: crate::render::atmosphere::Atmosphere,
    object: &Map<String, Value>,
) -> Result<crate::render::atmosphere::Atmosphere, String> {
    reject_unknown_keys(object, &["intensity", "night_brightness", "sun"])?;
    let mut patched = current;
    if let Some(intensity) = optional_number(object, "intensity")? {
        patched.intensity = intensity as f32;
    }
    if let Some(night) = optional_number(object, "night_brightness")? {
        patched.night_brightness = night as f32;
    }
    if let Some(sun) = object.get("sun") {
        patched.sun = sun_from_json(sun)?;
    }
    Ok(patched)
}

/// One `atmosphere::Sun` from its tagged-union JSON.
///
/// Every variant's arguments are required once its `mode` is chosen --
/// unlike the atmosphere's own fields, there is no current value to fall
/// back to inside a variant the sun was not already in, so a partial
/// `{"mode": "map", "lon_deg": 10}` has no honest meaning.
fn sun_from_json(value: &Value) -> Result<crate::render::atmosphere::Sun, String> {
    use crate::render::atmosphere::Sun;

    let object = value
        .as_object()
        .ok_or_else(|| format!("\"sun\" must be an object, got {}", type_name_of(value)))?;
    let mode = object
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| "\"sun\" needs a \"mode\" of now, utc, map or viewport".to_owned())?;
    match mode {
        "now" => {
            reject_unknown_keys(object, &["mode"])?;
            Ok(Sun::Now)
        }
        "utc" => {
            reject_unknown_keys(object, &["mode", "unix_seconds"])?;
            Ok(Sun::at_utc(number(object, "unix_seconds")?))
        }
        "map" => {
            reject_unknown_keys(object, &["mode", "lon_deg", "lat_deg"])?;
            Ok(Sun::Map {
                lon_deg: number(object, "lon_deg")?,
                lat_deg: number(object, "lat_deg")?,
            })
        }
        "viewport" => {
            reject_unknown_keys(object, &["mode", "tilt_deg", "bearing_deg"])?;
            Ok(Sun::Viewport {
                tilt_deg: number(object, "tilt_deg")? as f32,
                bearing_deg: number(object, "bearing_deg")? as f32,
            })
        }
        other => Err(format!(
            "unknown sun mode {other:?} -- one of now, utc, map or viewport"
        )),
    }
}

/// The JSON for one `Sun`.
///
/// `Sun::Now` reports as `{"mode": "now"}` rather than as the point it would
/// resolve to this instant, because that is the value a caller has to send
/// back to restore it: resolving it here would turn "track the clock" into
/// "stopped at 03:41:07Z" the moment anyone round-tripped the state.
fn sun_json(sun: crate::render::atmosphere::Sun) -> Value {
    use crate::render::atmosphere::Sun;

    match sun {
        Sun::Now => json!({ "mode": "now" }),
        Sun::Map { lon_deg, lat_deg } => {
            json!({ "mode": "map", "lon_deg": lon_deg, "lat_deg": lat_deg })
        }
        Sun::Viewport { tilt_deg, bearing_deg } => json!({
            "mode": "viewport",
            "tilt_deg": f32_json(tilt_deg),
            "bearing_deg": f32_json(bearing_deg),
        }),
    }
}

/// The whole of `map.get_space`'s answer, and what all four space writes
/// return.
///
/// A write answers with the state it produced rather than `{"ok": true}` so
/// that the response IS the argument that undoes it -- the round trip the
/// "change one thing, look, change it back" loop depends on. It also shows
/// the clamped values beside the raw ones, because a caller who set
/// `night_brightness: 1.5` has the terminator off as far as the screen is
/// concerned and would otherwise read their own `1.5` back and conclude the
/// night side was being dimmed.
///
/// `globe_transition` is here for the reason the declaration gives: at 0.0
/// none of this reaches a pixel, and without it a correct write and a dead
/// one are indistinguishable.
fn space_json(target: &MapView) -> Value {
    let space = target.space();
    let atmosphere = space.atmosphere;
    json!({
        "color": rgba_to_hex(space.color),
        "star_intensity": f32_json(space.star_intensity),
        "star_alpha": f32_json(space.star_alpha()),
        "star_size_scale": f32_json(space.star_size_scale),
        "atmosphere": {
            "intensity": f32_json(atmosphere.intensity),
            "night_brightness": f32_json(atmosphere.night_brightness),
            "night_brightness_clamped": f32_json(atmosphere.night_brightness_clamped()),
            "terminator": atmosphere.night_brightness_clamped() < 1.0,
            "sun": sun_json(atmosphere.sun),
        },
        // The SPACE transition (z3..z6), not the projection's (z11..z12):
        // the backdrop, air, stars and terminator fade on that one, and
        // `set_globe_enabled(false)` takes them off with the projection.
        "globe_transition": if target.globe_enabled() {
            crate::render::globe::space_transition_for_zoom(target.camera().zoom)
        } else {
            0.0
        },
    })
}

/// One `f32` as JSON, at `f32` precision rather than widened to `f64`.
///
/// `json!(0.1f32)` is `0.10000000149011612` -- arithmetically correct and
/// diagnostically useless. It matters here more than it would anywhere else
/// in this file, because these answers are meant to be read by a human
/// driving the map by hand AND fed straight back in to undo a change: the
/// value that comes out has to be the value that went in, or "change it
/// back" becomes "change it back to something very slightly different, and
/// hope".
///
/// So this emits the shortest decimal that parses back to the SAME `f32` --
/// Rust's own `Display`, which guarantees exactly that -- which is a
/// faithful rendering of the stored value, not a rounded one. A non-finite
/// value becomes `null`, because JSON has no spelling for it.
fn f32_json(value: f32) -> Value {
    value
        .to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

/// `#rrggbbaa`, always eight digits.
///
/// Always the long form, alpha included, so the string this hands back is
/// one `map.set_space` will accept verbatim -- a three-digit shorthand would
/// round-trip a colour to a different colour.
fn rgba_to_hex(color: crate::style::Rgba) -> String {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(color.r),
        byte(color.g),
        byte(color.b),
        byte(color.a)
    )
}

fn lon_lat_zoom(args: &Value) -> Result<(f64, f64, f64), String> {
    let object = as_object(args)?;
    Ok((number(object, "lon")?, number(object, "lat")?, number(object, "zoom")?))
}

fn feature_id_to_json(id: &crate::overlay::FeatureId) -> Value {
    match id {
        crate::overlay::FeatureId::String(text) => Value::String(text.clone()),
        crate::overlay::FeatureId::Number(number) => {
            serde_json::Number::from_f64(*number).map_or(Value::Null, Value::Number)
        }
    }
}

fn json_to_feature_id(value: &Value) -> Option<crate::overlay::FeatureId> {
    match value {
        Value::String(text) => Some(crate::overlay::FeatureId::String(text.clone())),
        Value::Number(number) => number.as_f64().map(crate::overlay::FeatureId::Number),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never fetches: every test here is about dispatch, and a `MapView`
    /// needs *a* source to be constructed.
    struct UnusedTileSource;

    #[async_trait::async_trait]
    impl crate::tile_source::TileSource for UnusedTileSource {
        async fn fetch(&self, _tile: crate::geo::TileId) -> anyhow::Result<Option<Vec<u8>>> {
            anyhow::bail!("UnusedTileSource never actually fetches")
        }
    }

    /// A real, standalone `MapView` -- no gpui app, no window, no GPU.
    /// Its runtime is leaked for the test's lifetime, the same shortcut
    /// `map_view`'s own unit tests take: nothing dispatched here spawns
    /// onto it.
    fn test_map_view() -> MapView {
        let runtime = tokio::runtime::Runtime::new().expect("a test runtime");
        let handle = runtime.handle().clone();
        std::mem::forget(runtime);
        MapView::new(
            103.86,
            1.28,
            12.0,
            1024.0,
            768.0,
            crate::style::Style { layers: Vec::new(), sprite: None, sky: Default::default() },
            std::sync::Arc::new(UnusedTileSource),
            handle,
        )
    }

    const ONE_FEATURE: &str = r##"{
        "type": "FeatureCollection",
        "features": [{
            "type": "Feature",
            "id": "route-1",
            "properties": { "line-color": "#ff0000", "line-width": 4 },
            "geometry": { "type": "LineString", "coordinates": [[103.85, 1.28], [103.87, 1.29]] }
        }]
    }"##;

    /// A call every declared function will accept. Hand-written, one per
    /// name, so that "routable" means the arm ran -- not merely that the
    /// catch-all was avoided.
    fn sample_args(name: &str) -> Value {
        match name {
            "map.set_camera" | "map.fly_to" => json!({ "lon": 0.0, "lat": 0.0, "zoom": 5.0 }),
            "map.query_features_at" => json!({ "x": 512.0, "y": 384.0 }),
            "map.set_features" => json!({ "geojson": ONE_FEATURE }),
            "map.set_feature_paint" => json!({
                "updates": [{ "id": "route-1", "paint": { "line-color": "#00ff00" } }]
            }),
            // `true`, not `false`: every name in `DISPATCHABLE` is called
            // for real against one shared `MapView` in
            // `every_declared_function_is_routable_by_agent_dispatch`, and
            // a sample that left the globe OFF would be a test fixture
            // silently changing what a LATER arm in that loop renders.
            "map.set_globe" => json!({ "enabled": true }),
            // One field each, deliberately: these functions exist so a term
            // can be moved WITHOUT naming the rest, and a sample that
            // supplied the whole struct would leave that unexercised here.
            "map.set_atmosphere" => json!({ "intensity": 0.0 }),
            "map.set_star_intensity" => json!({ "intensity": 0.4 }),
            "map.set_terminator" => json!({ "on": false }),
            "map.set_space" => json!({ "color": "#05070f" }),
            _ => json!({}),
        }
    }

    /// Every name `agent_dispatch` will route, in the same cfg shape the
    /// dispatcher is built in.
    ///
    /// Maintained BY HAND, and that is the point: it is the second opinion
    /// `every_dispatchable_name_is_declared` compares the declarations
    /// against. Deriving it from `agent_functions()` would make that test
    /// a tautology.
    const DISPATCHABLE: &[&str] = &[
        "map.get_camera",
        "map.list_layers",
        "map.get_diagnostics",
        "map.query_features_at",
        "map.get_stats",
        "map.get_space",
        "map.set_camera",
        "map.fly_to",
        "map.set_features",
        "map.set_feature_paint",
        "map.set_globe",
        "map.set_atmosphere",
        "map.set_star_intensity",
        "map.set_terminator",
        "map.set_space",
    ];

    /// Half one of the anti-drift guard, and the load-bearing half:
    /// nothing is advertised that cannot actually be CALLED.
    ///
    /// Not a name comparison -- every declared function is dispatched, for
    /// real, against a real `MapView`, and must return `Ok`. A declaration
    /// with no dispatch arm is the worse of the two drift failures: the
    /// agent sees the function, calls it, and gets "unknown function" back
    /// from the very crate that declared it.
    #[test]
    fn every_declared_function_is_routable_by_agent_dispatch() {
        let mut view = test_map_view();
        // Ordered so the paint update below has a feature to name: the
        // declaration order is read-before-write, which puts set_features
        // ahead of set_feature_paint already.
        for decl in agent_functions() {
            let result = agent_dispatch(&mut view, &decl.name, sample_args(&decl.name));
            let value = result.unwrap_or_else(|error| {
                panic!(
                    "{} is declared by agent_functions() but dispatching it failed: {error}",
                    decl.name
                )
            });
            assert!(
                value.is_object(),
                "{} must return a JSON object, so a caller can add fields to the response \
                 later without breaking one that reads it",
                decl.name
            );
        }
    }

    /// Half two: nothing is callable that was never advertised.
    ///
    /// A dispatch arm with no declaration is invisible to discovery -- the
    /// consumer's registry aggregates `agent_functions()` and nothing
    /// else, so the capability does not exist as far as any agent knows.
    #[test]
    fn every_dispatchable_name_is_declared() {
        let declared: Vec<String> =
            agent_functions().iter().map(|decl| decl.name.clone()).collect();
        let undeclared: Vec<&&str> = DISPATCHABLE
            .iter()
            .filter(|name| !declared.iter().any(|found| found == *name))
            .collect();
        assert!(
            undeclared.is_empty(),
            "these names are routed by agent_dispatch but are not declared by \
             agent_functions(): {undeclared:?}"
        );
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), declared.len(), "a name is declared twice: {declared:?}");
    }

    #[test]
    fn an_unknown_name_is_an_error_not_a_silent_success() {
        let mut view = test_map_view();
        let error = agent_dispatch(&mut view, "map.capture", json!({}))
            .expect_err("map.capture is deferred and must not dispatch");
        assert!(error.contains("unknown function"), "unhelpful error: {error}");
    }

    /// The read functions must answer from the map they are given, not
    /// from a default -- which is the one way a dispatcher can be wired to
    /// the wrong instance and still pass every schema test.
    #[test]
    fn a_read_answers_from_the_map_it_was_given() {
        let mut view = test_map_view();
        agent_dispatch(&mut view, "map.set_camera", json!({ "lon": -73.98, "lat": 40.76, "zoom": 9.5 }))
            .expect("set_camera dispatches");
        let camera = agent_dispatch(&mut view, "map.get_camera", json!({}))
            .expect("get_camera dispatches");
        assert_eq!(camera["lon"], json!(-73.98));
        assert_eq!(camera["lat"], json!(40.76));
        assert_eq!(camera["zoom"], json!(9.5));
    }

    /// `map.set_globe` must reach `MapView::set_globe_enabled` and report
    /// the transition the CAMERA is actually at -- not a constant, which is
    /// the shape a wrongly-wired arm would still satisfy.
    #[test]
    fn set_globe_flips_the_map_and_reports_the_transition_at_this_camera() {
        let mut view = test_map_view();
        assert!(view.globe_enabled(), "a fresh map has the globe ON");
        // Inside the z11-z12 projection band, so the two states report
        // different transitions and the round trip is observable.
        agent_dispatch(&mut view, "map.set_camera", json!({ "lon": 13.4, "lat": 52.5, "zoom": 11.133 }))
            .expect("set_camera dispatches");

        let off = agent_dispatch(&mut view, "map.set_globe", json!({ "enabled": false }))
            .expect("set_globe dispatches");
        assert_eq!(off["globe_enabled"], json!(false));
        assert_eq!(off["projection_transition"], json!(0.0));
        assert!(!view.globe_enabled(), "the map itself must be switched off");

        let on = agent_dispatch(&mut view, "map.set_globe", json!({ "enabled": true }))
            .expect("set_globe dispatches");
        assert_eq!(on["globe_enabled"], json!(true));
        let transition = on["projection_transition"].as_f64().expect("a number");
        assert!(
            transition > 0.85 && transition < 0.89,
            "z11.133 is 86.7% sphere, so an ON report of {transition} is not reading the camera"
        );
        assert!(view.globe_enabled());

        // A missing or non-boolean argument is an error, never a silent
        // default -- "set" functions that guess are how a map ends up in a
        // state nobody asked for.
        assert!(agent_dispatch(&mut view, "map.set_globe", json!({})).is_err());
        assert!(agent_dispatch(&mut view, "map.set_globe", json!({ "enabled": "off" })).is_err());
        assert!(view.globe_enabled(), "a rejected call must change nothing");
    }

    #[test]
    fn a_write_reports_what_it_wrote() {
        let mut view = test_map_view();
        let written = agent_dispatch(&mut view, "map.set_features", json!({ "geojson": ONE_FEATURE }))
            .expect("set_features dispatches");
        assert_eq!(written["feature_count"], json!(1));
        assert_eq!(written["ok"], json!(true));
        assert_eq!(
            view.overlay().generation(),
            written["generation"].as_u64().expect("a generation"),
            "the reported generation must be the store's"
        );
    }

    /// A GeoJSON object is as valid an argument as a GeoJSON string --
    /// declared as `["object", "string"]`, and an agent will send whichever
    /// its own serialiser produces.
    #[test]
    fn set_features_takes_the_collection_as_an_object_too() {
        let mut view = test_map_view();
        let document: Value = serde_json::from_str(ONE_FEATURE).expect("the fixture parses");
        let written = agent_dispatch(&mut view, "map.set_features", json!({ "geojson": document }))
            .expect("an object payload dispatches");
        assert_eq!(written["feature_count"], json!(1));
    }

    /// A rejected call must leave the map alone -- the consumer shows
    /// `safety` to an agent before it calls, and "write" is a promise that
    /// a FAILED write did not half-happen.
    #[test]
    fn a_rejected_write_changes_nothing() {
        let mut view = test_map_view();
        agent_dispatch(&mut view, "map.set_features", json!({ "geojson": ONE_FEATURE }))
            .expect("the good push lands");
        let before = view.overlay().generation();

        let error = agent_dispatch(&mut view, "map.set_features", json!({ "geojson": "not geojson" }))
            .expect_err("malformed GeoJSON must be rejected");
        assert!(!error.is_empty());
        assert_eq!(view.overlay().generation(), before, "a rejected push moved the generation");
        assert_eq!(view.overlay().features().len(), 1, "a rejected push changed the features");
    }

    #[test]
    fn a_paint_update_naming_an_unknown_feature_is_an_error() {
        let mut view = test_map_view();
        agent_dispatch(&mut view, "map.set_features", json!({ "geojson": ONE_FEATURE }))
            .expect("the push lands");
        agent_dispatch(
            &mut view,
            "map.set_feature_paint",
            json!({ "updates": [{ "id": "no-such-route", "paint": { "line-color": "#00ff00" } }] }),
        )
        .expect_err("an unknown feature id must be refused");
    }

    #[test]
    fn missing_and_mistyped_arguments_are_reported_by_name() {
        let mut view = test_map_view();
        let error = agent_dispatch(&mut view, "map.set_camera", json!({ "lat": 0.0, "zoom": 1.0 }))
            .expect_err("a missing lon must be refused");
        assert!(error.contains("lon"), "the error must name the argument: {error}");

        let error = agent_dispatch(&mut view, "map.set_camera", json!({ "lon": "west", "lat": 0.0, "zoom": 1.0 }))
            .expect_err("a non-numeric lon must be refused");
        assert!(error.contains("lon"), "the error must name the argument: {error}");

        let error = agent_dispatch(&mut view, "map.get_camera", json!("not an object"))
            .is_err();
        assert!(!error, "get_camera takes no arguments and must not care what it is sent");
    }

    /// The governing constraint, as an assertion: moving ONE atmosphere
    /// term leaves the other two exactly where they were.
    ///
    /// This is the whole diagnostic value of these functions. A caller who
    /// sets `intensity: 0` to find out whether the glow is drawing the arcs
    /// they are chasing has learned nothing if the same call also moved the
    /// night floor or the sun -- they would not know which change the screen
    /// was responding to.
    #[test]
    fn one_atmosphere_term_moves_without_disturbing_the_others() {
        let mut view = test_map_view();
        agent_dispatch(
            &mut view,
            "map.set_atmosphere",
            json!({ "night_brightness": 0.1, "sun": { "mode": "map", "lon_deg": 12.0, "lat_deg": -3.0 } }),
        )
        .expect("the setup patch dispatches");

        let after = agent_dispatch(&mut view, "map.set_atmosphere", json!({ "intensity": 0.0 }))
            .expect("intensity alone dispatches");

        assert_eq!(after["atmosphere"]["intensity"], json!(0.0));
        assert_eq!(
            after["atmosphere"]["night_brightness"],
            json!(0.1),
            "setting intensity moved the night floor"
        );
        assert_eq!(
            after["atmosphere"]["sun"],
            json!({ "mode": "map", "lon_deg": 12.0, "lat_deg": -3.0 }),
            "setting intensity moved the sun"
        );
        assert_eq!(
            after["star_intensity"],
            json!(0.0),
            "setting intensity touched the star field"
        );
    }

    /// The response to a write is the argument that undoes it, and the
    /// round trip has to actually close -- otherwise "change it back" is a
    /// claim rather than a procedure.
    #[test]
    fn a_write_answers_with_the_argument_that_undoes_it() {
        let mut view = test_map_view();
        let before = agent_dispatch(&mut view, "map.get_space", json!({}))
            .expect("get_space dispatches");

        agent_dispatch(
            &mut view,
            "map.set_space",
            json!({
                "color": "#112233ff",
                "star_intensity": 0.7,
                "star_size_scale": 2.0,
                "atmosphere": { "intensity": 0.3, "night_brightness": 0.05 },
            }),
        )
        .expect("the whole-struct patch dispatches");

        let restored = agent_dispatch(
            &mut view,
            "map.set_space",
            json!({
                "color": before["color"],
                "star_intensity": before["star_intensity"],
                "star_size_scale": before["star_size_scale"],
                "atmosphere": {
                    "intensity": before["atmosphere"]["intensity"],
                    "night_brightness": before["atmosphere"]["night_brightness"],
                    "sun": before["atmosphere"]["sun"],
                },
            }),
        )
        .expect("the read-back values are accepted verbatim");

        assert_eq!(restored["color"], before["color"]);
        assert_eq!(restored["star_intensity"], before["star_intensity"]);
        assert_eq!(restored["star_size_scale"], before["star_size_scale"]);
        assert_eq!(restored["atmosphere"], before["atmosphere"]);
    }

    /// `map.set_space` is partial too: the outer fields follow the same rule
    /// the nested atmosphere does.
    #[test]
    fn set_space_keeps_every_field_it_was_not_given() {
        let mut view = test_map_view();
        agent_dispatch(&mut view, "map.set_star_intensity", json!({ "intensity": 0.6 }))
            .expect("the star field is turned on");

        let after = agent_dispatch(&mut view, "map.set_space", json!({ "color": "#05070f" }))
            .expect("a colour-only patch dispatches");
        assert_eq!(after["color"], json!("#05070fff"), "the colour is reported in full");
        assert_eq!(
            after["star_intensity"],
            json!(0.6),
            "a colour-only patch reset the star field, which is the whole-struct \
             clobber this shape exists to prevent"
        );
        assert_eq!(after["atmosphere"]["night_brightness"], json!(1.0), "the default floor, untouched");
    }

    /// The `T` key's flip, and the one difference from it that the
    /// declaration promises: `on` returns to the terminator's own floor
    /// (0.25), not to a remembered one.
    #[test]
    fn the_terminator_flips_off_and_back_on() {
        let mut view = test_map_view();
        let initial = agent_dispatch(&mut view, "map.get_space", json!({}))
            .expect("get_space dispatches");
        assert_eq!(
            initial["atmosphere"]["terminator"],
            json!(false),
            "the engine default is the terminator OFF (the 2026-09-29 ruling)"
        );

        let on = agent_dispatch(&mut view, "map.set_terminator", json!({ "on": true }))
            .expect("switching it on dispatches");
        assert_eq!(on["atmosphere"]["terminator"], json!(true));
        assert_eq!(
            on["atmosphere"]["night_brightness"],
            json!(0.25),
            "`on: true` from the default must actually dim the night side, not keep 1.0"
        );

        let off = agent_dispatch(&mut view, "map.set_terminator", json!({ "on": false }))
            .expect("switching it off dispatches");
        assert_eq!(off["atmosphere"]["terminator"], json!(false));
        assert_eq!(
            off["atmosphere"]["night_brightness"],
            json!(1.0),
            "off is exactly 1.0 -- the value that means no ground shading, not a large \
             number that clamps there"
        );

        let back = agent_dispatch(&mut view, "map.set_terminator", json!({ "on": true }))
            .expect("switching it back on dispatches");
        assert_eq!(back["atmosphere"]["terminator"], json!(true));
        assert_eq!(back["atmosphere"]["night_brightness"], json!(0.25));

        let floor = agent_dispatch(
            &mut view,
            "map.set_terminator",
            json!({ "on": true, "night_brightness": 0.05 }),
        )
        .expect("a named floor dispatches");
        assert_eq!(
            floor["atmosphere"]["night_brightness"],
            json!(0.05),
            "a non-default floor has to be reachable -- GPUI_MAP_NIGHT can pick one"
        );

        let again = agent_dispatch(&mut view, "map.set_terminator", json!({ "on": true }))
            .expect("switching an already-on terminator on dispatches");
        assert_eq!(
            again["atmosphere"]["night_brightness"],
            json!(0.05),
            "`on: true` on an already-on terminator reset the floor, which is this \
             function moving a term the caller never named"
        );

        let zero = agent_dispatch(
            &mut view,
            "map.set_terminator",
            json!({ "on": true, "night_brightness": 0.0 }),
        )
        .expect("a black night side dispatches");
        assert_eq!(
            zero["atmosphere"]["night_brightness"],
            json!(0.0),
            "GPUI_MAP_NIGHT=0 is the truthful night side and has to be reachable"
        );
        assert_eq!(
            zero["atmosphere"]["terminator"],
            json!(true),
            "0.0 is the terminator fully ON, not off"
        );
    }

    /// Neither the terminator nor the star field may reach across into the
    /// other terms -- the same isolation rule, on the two shorthands.
    #[test]
    fn the_shorthands_touch_exactly_one_term_each() {
        let mut view = test_map_view();
        agent_dispatch(&mut view, "map.set_atmosphere", json!({ "intensity": 0.42 }))
            .expect("the setup patch dispatches");

        let after = agent_dispatch(&mut view, "map.set_terminator", json!({ "on": false }))
            .expect("the terminator flips");
        assert_eq!(after["atmosphere"]["intensity"], json!(0.42), "the flip moved the glow");

        let after = agent_dispatch(&mut view, "map.set_star_intensity", json!({ "intensity": 1.0 }))
            .expect("the star field is turned up");
        assert_eq!(
            after["atmosphere"]["intensity"],
            json!(0.42),
            "the star field moved the glow"
        );
        assert_eq!(after["atmosphere"]["night_brightness"], json!(1.0));
    }

    /// The raw value a caller set and the value the shader is actually
    /// handed are different numbers whenever the caller went out of range,
    /// and reporting only the first would have them diagnose a state the
    /// map is not in.
    #[test]
    fn out_of_range_values_are_reported_raw_and_clamped() {
        let mut view = test_map_view();
        let after = agent_dispatch(&mut view, "map.set_star_intensity", json!({ "intensity": 4.0 }))
            .expect("an out-of-range intensity is accepted, like the field itself");
        assert_eq!(after["star_intensity"], json!(4.0));
        assert_eq!(after["star_alpha"], json!(1.0), "the shader sees 1.0");

        let after =
            agent_dispatch(&mut view, "map.set_atmosphere", json!({ "night_brightness": 1.5 }))
                .expect("an out-of-range night floor is accepted");
        assert_eq!(after["atmosphere"]["night_brightness"], json!(1.5));
        assert_eq!(after["atmosphere"]["night_brightness_clamped"], json!(1.0));
        assert_eq!(
            after["atmosphere"]["terminator"],
            json!(false),
            "1.5 clamps to 1.0, which is the terminator OFF -- a caller reading their \
             own 1.5 back would conclude the night side was being dimmed"
        );
    }

    /// Every sun mode survives a round trip through its own JSON, and
    /// `Now` reports as `now` rather than as the instant it would resolve
    /// to -- otherwise saving and restoring the state would quietly stop
    /// the clock.
    #[test]
    fn every_sun_mode_round_trips() {
        let mut view = test_map_view();
        for sun in [
            json!({ "mode": "viewport", "tilt_deg": 55.0, "bearing_deg": -45.0 }),
            json!({ "mode": "map", "lon_deg": -71.5, "lat_deg": 18.0 }),
            json!({ "mode": "now" }),
        ] {
            let after =
                agent_dispatch(&mut view, "map.set_atmosphere", json!({ "sun": sun.clone() }))
                    .expect("a sun dispatches");
            assert_eq!(after["atmosphere"]["sun"], sun, "the sun did not round-trip");
        }

        // `utc` is a constructor, not a variant: it resolves to a subsolar
        // point, so it comes back as `map`. Asserted rather than left to be
        // discovered, because a caller expecting `utc` back would read the
        // response as a failure.
        let after = agent_dispatch(
            &mut view,
            "map.set_atmosphere",
            json!({ "sun": { "mode": "utc", "unix_seconds": 1_700_000_000.0 } }),
        )
        .expect("an instant dispatches");
        assert_eq!(after["atmosphere"]["sun"]["mode"], json!("map"));
        assert!(after["atmosphere"]["sun"]["lon_deg"].is_number());
    }

    /// The failure path, and the one that matters most here: on functions
    /// where every field is optional, a name the dispatcher does not know
    /// must be an error. Accepting it would return the state unchanged and
    /// report success, and a caller would conclude the term they meant to
    /// test was not the cause.
    #[test]
    fn a_misspelled_space_argument_is_an_error_not_a_silent_no_op() {
        let mut view = test_map_view();
        for (name, typo, args) in [
            ("map.set_atmosphere", "intesity", json!({ "intesity": 0.0 })),
            ("map.set_space", "starintensity", json!({ "starintensity": 1.0 })),
            (
                "map.set_star_intensity",
                "colour",
                json!({ "intensity": 1.0, "colour": "#fff" }),
            ),
            ("map.set_terminator", "night", json!({ "on": true, "night": 0.1 })),
        ] {
            let Err(error) = agent_dispatch(&mut view, name, args) else {
                panic!("{name} accepted {typo:?}, an argument it does not have");
            };
            assert!(
                error.contains(typo),
                "{name} refused {typo:?} without saying which name was wrong: {error}"
            );
        }

        // And the nested case, which is the one a shape written twice would
        // get wrong: `map.set_space`'s `atmosphere` must be validated by the
        // same code `map.set_atmosphere` uses.
        let error =
            agent_dispatch(&mut view, "map.set_space", json!({ "atmosphere": { "sun_deg": 1.0 } }))
                .expect_err("a nested typo must be refused too");
        assert!(error.contains("sun_deg"), "the error must name the argument: {error}");
    }

    /// The rest of the validation surface: a required argument missing, a
    /// field of the wrong type, and a sun mode that does not exist. Each
    /// error names what was wrong.
    #[test]
    fn malformed_space_arguments_are_reported_by_name() {
        let mut view = test_map_view();

        let error = agent_dispatch(&mut view, "map.set_star_intensity", json!({}))
            .expect_err("a missing intensity must be refused");
        assert!(error.contains("intensity"), "the error must name the argument: {error}");

        let error =
            agent_dispatch(&mut view, "map.set_star_intensity", json!({ "intensity": "bright" }))
                .expect_err("a non-numeric intensity must be refused");
        assert!(error.contains("intensity"), "the error must name the argument: {error}");

        let error = agent_dispatch(&mut view, "map.set_terminator", json!({}))
            .expect_err("a missing on must be refused");
        assert!(error.contains("on"), "the error must name the argument: {error}");

        let error = agent_dispatch(&mut view, "map.set_terminator", json!({ "on": "yes" }))
            .expect_err("a non-boolean on must be refused");
        assert!(error.contains("on"), "the error must name the argument: {error}");

        let error =
            agent_dispatch(&mut view, "map.set_atmosphere", json!({ "sun": { "mode": "dusk" } }))
                .expect_err("an unknown sun mode must be refused");
        assert!(error.contains("dusk"), "the error must name the mode: {error}");

        let error = agent_dispatch(
            &mut view,
            "map.set_atmosphere",
            json!({ "sun": { "mode": "map", "lon_deg": 10.0 } }),
        )
        .expect_err("a sun mode missing half its point must be refused");
        assert!(error.contains("lat_deg"), "the error must name the argument: {error}");

        let error = agent_dispatch(&mut view, "map.set_space", json!({ "color": "rebeccapurple" }))
            .expect_err("a named CSS colour must be refused");
        assert!(error.contains("color"), "the error must name the argument: {error}");
    }

    /// `safety: write` promises a FAILED write did not half-happen. The
    /// patch here has one good field and one bad one, in that order, so a
    /// dispatcher that applied as it parsed would leave the good half
    /// standing -- and a caller running "change one thing and change it
    /// back" would have no idea which half to put back.
    #[test]
    fn a_rejected_space_write_changes_nothing() {
        let mut view = test_map_view();
        let before = agent_dispatch(&mut view, "map.get_space", json!({}))
            .expect("get_space dispatches");

        agent_dispatch(
            &mut view,
            "map.set_atmosphere",
            json!({ "intensity": 0.0, "sun": { "mode": "nowhere" } }),
        )
        .expect_err("the bad sun must sink the whole patch");

        agent_dispatch(
            &mut view,
            "map.set_space",
            json!({ "star_intensity": 1.0, "color": "not a colour" }),
        )
        .expect_err("the bad colour must sink the whole patch");

        let after = agent_dispatch(&mut view, "map.get_space", json!({}))
            .expect("get_space dispatches");
        assert_eq!(after, before, "a rejected write left something behind");
    }

    /// The same instance check the camera read gets: a space read must
    /// answer from the map it was handed, not from `Space::default()`.
    #[test]
    fn the_space_read_answers_from_the_map_it_was_given() {
        let mut view = test_map_view();
        view.set_star_intensity(0.33);
        let space = agent_dispatch(&mut view, "map.get_space", json!({}))
            .expect("get_space dispatches");
        assert_eq!(space["star_intensity"], json!(0.33));
    }

    /// `globe_transition` travels with every space answer, and it is not
    /// decoration: the test view opens at zoom 12, where the correct
    /// behaviour of a working setter is that NOTHING on screen moves.
    #[test]
    fn a_space_answer_says_whether_any_of_it_can_reach_a_pixel() {
        let mut view = test_map_view();
        let flat = agent_dispatch(&mut view, "map.set_atmosphere", json!({ "intensity": 0.0 }))
            .expect("the write dispatches");
        assert_eq!(
            flat["globe_transition"],
            json!(0.0),
            "zoom 12 is flat Mercator, and a caller must be told so rather than \
             concluding the setter is broken"
        );

        agent_dispatch(&mut view, "map.set_camera", json!({ "lon": 0.0, "lat": 0.0, "zoom": 2.0 }))
            .expect("set_camera dispatches");
        let globe = agent_dispatch(&mut view, "map.get_space", json!({}))
            .expect("get_space dispatches");
        assert!(
            globe["globe_transition"].as_f64().expect("a number") > 0.0,
            "zoom 2 is a globe, so the same state now reaches pixels"
        );
    }

    #[test]
    fn every_declaration_is_well_formed() {
        for decl in agent_functions() {
            assert!(
                decl.name.starts_with(NAMESPACE),
                "{:?} must carry the {NAMESPACE:?} namespace so a host can route on it",
                decl.name
            );
            assert!(!decl.description.is_empty(), "{} has no description", decl.name);
            assert_eq!(
                decl.params.get("type").and_then(Value::as_str),
                Some("object"),
                "{}'s params schema must be an object schema -- a call's arguments are \
                 always a JSON object",
                decl.name
            );
            assert!(
                decl.returns.is_object(),
                "{} must declare a returns schema; MCP does not standardise one, which \
                 is exactly why it is required here",
                decl.name
            );
            assert_eq!(
                decl.crate_version, CRATE_VERSION,
                "the crate version must travel with every declaration"
            );
        }
    }

    /// The consumer surfaces `safety` to an agent before it calls, so a
    /// mis-marked function is a safety claim this crate made and broke.
    #[test]
    fn safety_is_marked_honestly_per_function() {
        let by_name: std::collections::HashMap<String, Safety> =
            agent_functions().into_iter().map(|decl| (decl.name, decl.safety)).collect();
        for read in [
            "map.get_camera",
            "map.list_layers",
            "map.get_diagnostics",
            "map.query_features_at",
            "map.get_stats",
            "map.get_space",
        ] {
            assert_eq!(by_name.get(read), Some(&Safety::Read), "{read} observes only");
        }
        for write in [
            "map.set_camera",
            "map.fly_to",
            "map.set_features",
            "map.set_feature_paint",
            // Write, not Destructive: it changes what is drawn and is
            // undone exactly by the opposite call, with no state the
            // interface cannot put back.
            "map.set_globe",
            // Write, not destructive: every one of these answers with the
            // state it produced, and that answer is an argument this same
            // interface accepts -- so there is always a way back.
            "map.set_atmosphere",
            "map.set_star_intensity",
            "map.set_terminator",
            "map.set_space",
        ] {
            assert_eq!(by_name.get(write), Some(&Safety::Write), "{write} changes the map");
        }
    }

    #[test]
    fn declarations_serialise_in_the_agreed_shape() {
        let decls = serde_json::to_value(agent_functions()).expect("declarations serialise");
        let first = &decls[0];
        let mut keys: Vec<&str> = first
            .as_object()
            .expect("a declaration is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["crate_version", "description", "name", "params", "returns", "safety"],
            "the declaration shape is agreed with the consumer's registry: MCP's \
             name/description/params, plus returns and safety, plus provenance"
        );
        assert_eq!(first["safety"], json!("read"), "safety serialises as its lowercase name");
        assert_eq!(first["crate_version"], json!(env!("CARGO_PKG_VERSION")));
    }

    /// The hard constraint agreed with the consumer: a crate that links
    /// `gpui-maps` must not gain an inbound listener.
    ///
    /// The dev-harness HTTP endpoint lives in
    /// `kit/src/agentic.rs` and its server crates are
    /// `[dev-dependencies]`, which cargo does not propagate to dependants.
    /// That is what makes "zero inbound network surface" structural rather
    /// than a promise in a comment -- and this test is what stops someone
    /// moving the line one section up while wiring a second example.
    ///
    /// `gpui-agentic-http` is named alongside `tiny_http` because it *is*
    /// a listener by transitive dependency -- it wraps `tiny_http` and
    /// binds the socket itself. A crate that opens a port is a listener
    /// however thin the wrapper is, and putting the wrapper under
    /// `[dependencies]` would hand every consumer the port just as
    /// directly.
    #[test]
    fn the_http_server_never_reaches_a_consumers_dependency_tree() {
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        )
        .expect("Cargo.toml must be readable");

        // Any crate that can accept a connection. Not exhaustive, and it
        // does not need to be: it names what a future author would
        // plausibly reach for, and the alternative -- a whitelist -- would
        // fail on every ordinary dependency added from now on.
        const LISTENERS: &[&str] = &[
            "tiny_http",
            "gpui-agentic-http",
            "axum",
            "hyper",
            "warp",
            "actix-web",
            "tide",
            "rouille",
            "poem",
        ];

        let mut section = String::new();
        for line in manifest.lines() {
            let line = line.trim();
            if let Some(header) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
                section = header.to_owned();
                continue;
            }
            let Some(name) = line.split(['=', ' ']).next().filter(|name| !name.is_empty()) else {
                continue;
            };
            if !LISTENERS.contains(&name) {
                continue;
            }
            assert!(
                section.ends_with("dev-dependencies"),
                "{name} is declared under [{section}]. An HTTP server must only ever be a \
                 dev-dependency: a consumer linking gpui-maps must not gain a listener. \
                 The dev harness belongs in an example -- see \
                 kit/src/agentic.rs."
            );
        }

        // And the guard itself must be load-bearing: if the harness's own
        // crate ever stops being declared at all, this test would pass
        // vacuously forever.
        for crate_name in ["tiny_http", "gpui-agentic-http"] {
            assert!(
                manifest.contains(crate_name),
                "{crate_name} is gone from Cargo.toml -- if the dev harness was removed, \
                 remove this test with it rather than leaving it passing on nothing"
            );
        }
    }

    /// No module in `src/` may open a socket. The dev harness is an
    /// example; the library is not allowed to grow one quietly.
    #[test]
    fn the_library_binds_no_socket() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut files = vec![root];
        while let Some(path) = files.pop() {
            if path.is_dir() {
                for entry in std::fs::read_dir(&path).expect("src/ must be readable") {
                    files.push(entry.expect("a readable dir entry").path());
                }
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a readable .rs file");
            for (index, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                // Comments and doc comments discuss the constraint at
                // length; it is code that would break it.
                if trimmed.starts_with("//") {
                    continue;
                }
                // Spelled in halves so this scan does not find its own
                // needles in this very file.
                let binds = [
                    concat!("Tcp", "Listener"),
                    concat!("Udp", "Socket"),
                    concat!("Unix", "Listener"),
                ]
                .iter()
                .any(|needle| trimmed.contains(needle));
                if binds {
                    offenders.push(format!("{}:{}: {trimmed}", path.display(), index + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the library must have zero inbound network surface, and these lines bind or \
             accept:\n{}",
            offenders.join("\n")
        );
    }

    /// `map.capture` was deferred by agreement until the allocation
    /// counters have done their job. This fails if someone adds it early.
    #[test]
    fn map_capture_is_still_deferred() {
        assert!(
            !agent_functions().iter().any(|decl| decl.name == "map.capture"),
            "map.capture is deferred -- see the module doc comment"
        );
    }
}

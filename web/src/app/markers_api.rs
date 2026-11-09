//! The page-facing camera-and-markers API, for a host page that embeds the
//! map and drives it from its own UI (an address-search test page, say):
//!
//! ```js
//! gpuiMap.flyTo(lon, lat, zoom, durationMs?)           // default duration: by distance
//! gpuiMap.setMarkers([{lon, lat, label, kind: "address" | "arrival"}])
//! gpuiMap.clearMarkers()
//! gpuiMap.fitMarkers(pad?, maxZoom?, durationMs?)     // -> bool; pad: px or [t, r, b, l]
//! gpuiMap.markerAt(cssX, cssY)                        // -> index or -1
//! ```
//!
//! The dots are engine point sources. The labels are DOM text in the
//! basemap's place-label look, moved every drawn frame to their projected
//! points (`place_labels`, called by `MapApp::draw`): this shell's
//! overlay path draws no overlay text of its own.
//!
//! Plumbing only: parsing, the layers, the paint, the pick mapping, the
//! label offsets and the framing are `crate::overlay::markers`, tested
//! there.

use super::*;
use crate::overlay::host::markers::{self, Marker, MarkerLayer, LABEL_STYLE};

/// The labels' container, over the canvas and under the page's chrome.
const LABELS_ID: &str = "gpui-map-marker-labels";
/// The face the engine labels with, registered for CSS from the same files.
const LABEL_FAMILY: &str = "gpui-map-label";

#[derive(Default)]
struct MarkerSet {
    markers: Vec<Marker>,
    layers: Vec<MarkerLayer>,
    /// One element per labelled marker: `(marker index, element)`.
    labels: Vec<(usize, web_sys::HtmlElement)>,
    /// The camera and canvas origin the labels were last placed for.
    placed_at: std::cell::Cell<Option<(crate::render::Camera, f32, f32)>>,
}

thread_local! {
    static MARKERS: RefCell<MarkerSet> = RefCell::new(MarkerSet::default());
}

/// The default `fitMarkers` duration when the page passes none.
const FLY_MS: f64 = 900.0;

fn ease(a: &mut MapApp, lon: f64, lat: f64, zoom: f64, duration_ms: f64) {
    a.camera_control.ease_to(
        CameraTarget { center: Some((lon, lat)), zoom: Some(zoom.clamp(MIN_ZOOM, MAX_ZOOM)), ..CameraTarget::default() },
        crate::camera_control::ease_duration_from_ms(duration_ms),
    );
    a.pan_inertia = None;
    a.needs_redraw = true;
}

/// Flies the camera to `(lon, lat)` at `zoom`, pitch and bearing kept:
/// MapLibre's `flyTo` path (`CameraController::fly_to`), rising over a
/// long move. `durationMs` defaults to MapLibre's own, from the path's
/// length; 0 jumps.
#[wasm_bindgen(js_name = flyTo)]
pub fn fly_to(lon: f64, lat: f64, zoom: f64, duration_ms: Option<f64>) {
    with_app(|a| {
        a.camera_control.fly_to(
            CameraTarget { center: Some((lon, lat)), zoom: Some(zoom.clamp(MIN_ZOOM, MAX_ZOOM)), ..CameraTarget::default() },
            duration_ms.map(crate::camera_control::ease_duration_from_ms),
        );
        a.pan_inertia = None;
        a.needs_redraw = true;
    });
}

/// The labels' container, created (with its `@font-face`) on first use.
fn labels_root(document: &web_sys::Document) -> Option<web_sys::HtmlElement> {
    if let Some(el) = document.get_element_by_id(LABELS_ID) {
        return el.dyn_into().ok();
    }
    let style = document.create_element("style").ok()?;
    let halo = LABEL_STYLE.halo_px;
    let h = LABEL_STYLE.halo;
    style.set_text_content(Some(&format!(
        "@font-face {{ font-family: {LABEL_FAMILY}; src: url(./NotoSans-Regular.latin-subset.ttf); }}\n\
         @font-face {{ font-family: {LABEL_FAMILY}-jp; src: url(./NotoSansJP-Regular.japan-subset.otf); }}\n\
         #{LABELS_ID} {{ position: fixed; left: 0; top: 0; width: 0; height: 0; z-index: 1; pointer-events: none; }}\n\
         #{LABELS_ID} > span {{ position: absolute; left: 0; top: 0; white-space: nowrap; will-change: transform;\n\
           font: 400 {size}px/1.2 {LABEL_FAMILY}, {LABEL_FAMILY}-jp, \"Noto Sans JP\", sans-serif; color: {color};\n\
           text-shadow: {halo}px 0 {h}, -{halo}px 0 {h}, 0 {halo}px {h}, 0 -{halo}px {h},\n\
             {halo}px {halo}px {h}, -{halo}px {halo}px {h}, {halo}px -{halo}px {h}, -{halo}px -{halo}px {h}; }}",
        size = LABEL_STYLE.size_px,
        color = LABEL_STYLE.color,
    )));
    // In the body: `<style>` applies from there too, and `head()` would cost
    // a web-sys feature this crate does not otherwise need.
    document.body()?.append_child(&style).ok()?;
    let root: web_sys::HtmlElement = document.create_element("div").ok()?.dyn_into().ok()?;
    root.set_id(LABELS_ID);
    root.set_attribute("aria-hidden", "true").ok()?;
    // Right after the canvas, so the page's own chrome (later in the body,
    // z-index above 1) stays on top of the labels.
    match document.get_element_by_id("map") {
        Some(canvas) => canvas.after_with_node_1(&root).ok()?,
        None => document.body()?.append_child(&root).map(|_| ()).ok()?,
    }
    Some(root)
}

/// Moves every marker label to its projected point on this frame; hides a
/// label whose point the planet hides. Called at the top of every
/// `MapApp::draw` (the camera that frame draws with), and once by
/// `setMarkers`.
pub(super) fn place_labels(a: &MapApp) {
    MARKERS.with(|set| {
        let set = set.borrow();
        if set.labels.is_empty() {
            return;
        }
        let camera = a.camera_control.camera();
        let frame = crate::render::globe::globe_frame(camera);
        let anchors = crate::render::globe::GlobeAnchors::new(&frame);
        let projector = crate::render::globe::WorldProjector::new(camera, &anchors);
        let rect = a.canvas.get_bounding_client_rect();
        let (ox, oy) = (rect.left() as f32, rect.top() as f32);
        // Moved only when the camera or the canvas did: an idle map costs
        // one comparison a tick.
        let key = (*camera, ox, oy);
        if set.placed_at.get() == Some(key) {
            return;
        }
        set.placed_at.set(Some(key));
        for (i, el) in &set.labels {
            let m = &set.markers[*i];
            let at = projector.project(crate::overlay::host::lon_lat_to_world(m.lon, m.lat));
            let style = el.style();
            match at {
                Some((x, y)) => {
                    let (dx, dy) = markers::label_offset_px(m.kind);
                    let _ = style.set_property(
                        "transform",
                        &format!("translate({:.1}px, {:.1}px) translate(-50%, -50%)", ox + x + dx, oy + y + dy),
                    );
                    let _ = style.set_property("visibility", "visible");
                }
                None => {
                    let _ = style.set_property("visibility", "hidden");
                }
            }
        }
    });
}

fn remove_layers(a: &mut MapApp, layers: &[MarkerLayer]) {
    for l in layers {
        a.overlays.remove_source(&l.source);
    }
}

/// Replaces every marker with `points` (`[{lon, lat, label?, kind?}]`,
/// `kind` `"address"` (default) or `"arrival"`). Throws on a bad entry,
/// leaving the previous markers in place.
#[wasm_bindgen(js_name = setMarkers)]
pub fn set_markers(points: JsValue) -> Result<(), JsValue> {
    let text = js_sys::JSON::stringify(&points).map_err(|_| JsValue::from_str("markers: not JSON-serialisable"))?;
    let parsed = markers::parse_markers(&String::from(text)).map_err(|e| JsValue::from_str(&e))?;
    let layers = markers::marker_layers(&parsed);
    let document = web_sys::window().and_then(|w| w.document()).ok_or_else(|| JsValue::from_str("no document"))?;
    with_app(|a| {
        let old = MARKERS.with(|s| std::mem::take(&mut s.borrow_mut().layers));
        remove_layers(a, &old);
        for l in &layers {
            let no_cluster = crate::overlay::host::ClusterOptions { cluster: false, ..crate::overlay::host::ClusterOptions::default() };
            a.overlays.set_points(&l.source, &l.lon_lat, &l.paint, no_cluster).map_err(|e| JsValue::from_str(&e.to_string()))?;
            // A marker inside a building's footprint must not sit under
            // its translucent roof.
            a.overlays.set_source_above_buildings(&l.source, true);
        }
        a.needs_redraw = true;
        Ok::<(), JsValue>(())
    })
    .unwrap_or_else(|| Err(JsValue::from_str("map not ready")))?;
    let root = labels_root(&document).ok_or_else(|| JsValue::from_str("markers: could not create the label layer"))?;
    root.set_text_content(None);
    let mut labels = Vec::new();
    for (i, m) in parsed.iter().enumerate() {
        let text = m.label.trim();
        if text.is_empty() {
            continue;
        }
        let el: web_sys::HtmlElement = document.create_element("span")?.dyn_into()?;
        el.set_text_content(Some(text));
        el.set_attribute("data-kind", m.kind.name())?;
        el.style().set_property("visibility", "hidden")?;
        root.append_child(&el)?;
        labels.push((i, el));
    }
    MARKERS.with(|s| *s.borrow_mut() = MarkerSet { markers: parsed, layers, labels, placed_at: Default::default() });
    with_app(|a| place_labels(a));
    Ok(())
}

/// Removes every marker and its label.
#[wasm_bindgen(js_name = clearMarkers)]
pub fn clear_markers() {
    let old = MARKERS.with(|s| std::mem::take(&mut *s.borrow_mut()));
    for (_, el) in &old.labels {
        el.remove();
    }
    with_app(|a| {
        remove_layers(a, &old.layers);
        a.needs_redraw = true;
    });
}

/// Eases the camera to frame every marker with `pad` clear -- a number of
/// px for every side (default 80), or `[top, right, bottom, left]` for the
/// room a page's floating chrome takes -- no closer than `maxZoom`
/// (default 17). `false` with no markers set.
#[wasm_bindgen(js_name = fitMarkers)]
pub fn fit_markers(pad: JsValue, max_zoom: Option<f64>, duration_ms: Option<f64>) -> bool {
    let pad: markers::Padding = if let Some(p) = pad.as_f64() {
        [p; 4]
    } else if js_sys::Array::is_array(&pad) {
        let a = js_sys::Array::from(&pad);
        let at = |i: u32| a.get(i).as_f64().unwrap_or(0.0);
        [at(0), at(1), at(2), at(3)]
    } else {
        [80.0; 4]
    };
    with_app(|a| {
        let camera = *a.camera_control.camera();
        let fit = MARKERS.with(|s| {
            markers::fit_markers(
                &s.borrow().markers,
                f64::from(camera.viewport_width_px),
                f64::from(camera.viewport_height_px),
                pad,
                max_zoom.unwrap_or(17.0),
            )
        });
        let Some((lon, lat, zoom)) = fit else { return false };
        ease(a, lon, lat, zoom, duration_ms.unwrap_or(FLY_MS));
        true
    })
    .unwrap_or(false)
}

/// The index (into the last `setMarkers` list) of the marker under CSS
/// pixel `(x, y)` of the canvas, or -1. A page calls it from its own click
/// handler: the map does not swallow clicks, so this is the click callback.
#[wasm_bindgen(js_name = markerAt)]
pub fn marker_at(x: f32, y: f32) -> i32 {
    with_app(|a| {
        let camera = *a.camera_control.camera();
        let hit = a.overlays.feature_at(&camera, x, y)?;
        MARKERS.with(|s| markers::marker_for_pick(&s.borrow().layers, &hit.source, hit.index))
    })
    .flatten()
    .map_or(-1, |i| i as i32)
}

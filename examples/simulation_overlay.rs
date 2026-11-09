//! Loads a fleet simulation GeoJSON over the basemap.
//!
//! The document is named by `SIMULATION_GEOJSON`; nothing is embedded here,
//! because the dataset this was built against is operational data that does
//! not belong in this repository.
//!
//! It also shows the shape of the gap between this overlay and a production
//! mapbox-gl style: that style drives paint from feature properties
//! (`["get", "colour"]`) and interpolates line width over zoom, neither of
//! which this overlay supports yet, so `translate` below does by hand what
//! expressions would do for it -- rewriting property names and splitting
//! each route into the casing and fill features the style's two line layers
//! draw.
//!
//! Run with:
//!   SIMULATION_GEOJSON=geojsons/simulation.json cargo run --release --example simulation_overlay

use std::sync::Arc;

use gpui::{px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_maps::map_view::MapView;
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;
use serde_json::{json, Map, Value};

/// The zoom this example opens at. Widths below are evaluated here, because
/// the overlay cannot interpolate paint over zoom yet -- so a constant has
/// to be picked for *some* zoom, and picking the one on screen is the only
/// choice that makes a side-by-side with mapbox-gl honest.
const OPENING_ZOOM: f64 = 12.0;

fn translate(raw: &str) -> anyhow::Result<String> {
    let source: Value = serde_json::from_str(raw)?;
    let features = source["features"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected a GeoJSON FeatureCollection"))?;

    // Two passes, not one interleaved pass. A mapbox-gl style draws LAYERS:
    // the whole `vehicle-route-outline` layer, then the whole
    // `vehicle-route-line` layer. Emitting casing-then-core per feature
    // instead lets a later route's casing paint over an earlier route's
    // core, so wherever routes overlap the one underneath appears to grow a
    // thick dark border. This overlay has no layer concept -- draw order is
    // array order -- so a caller emulates layers by ordering the features,
    // which is what these two passes do.
    let mut casings: Vec<Value> = Vec::new();
    let mut cores: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::with_capacity(features.len() * 2);
    for feature in features {
        let properties = feature["properties"].as_object().cloned().unwrap_or_default();
        let geometry = feature["geometry"].clone();
        let colour = properties.get("colour").and_then(Value::as_str).unwrap_or("#3b82f6");
        let id = properties.get("vehicle").cloned().unwrap_or(Value::Null);

        let emit = |props: Map<String, Value>| {
            json!({"type": "Feature", "id": id, "geometry": geometry, "properties": props})
        };

        if geometry["type"].as_str() == Some("LineString") {
            // Casing first, route second: draw order is array order, which
            // is how the style's two line layers stack.
            let outline =
                properties.get("outline_colour").and_then(Value::as_str).unwrap_or("#ffffff");
            casings.push(emit(Map::from_iter([
                ("line-color".to_string(), json!(outline)),
                ("line-width".to_string(), json!(["interpolate", ["linear"], ["zoom"], 8, 2.4, 18, 7.2])),
            ])));
            cores.push(emit(Map::from_iter([
                ("line-color".to_string(), json!(colour)),
                ("line-width".to_string(), json!(["interpolate", ["linear"], ["zoom"], 8, 1.0, 18, 3.0])),
            ])));
        } else {
            // A white badge with the route's own colour as its border --
            // the shape every transit map draws a stop as, and the reason
            // the pin is an SDF sprite: ONE image, recoloured per route,
            // rather than one baked PNG per colour.
            let mut pin = Map::from_iter([
                // No dot under the pin: the icon IS the stop. Without this
                // the default circle draws too and shows around the badge.
                ("circle-radius".to_string(), json!(0.0)),
                ("icon-image".to_string(), json!(STOP_PIN_IMAGE)),
                ("icon-color".to_string(), json!("#ffffff")),
                ("icon-halo-color".to_string(), json!(colour)),
                ("icon-halo-width".to_string(), json!(2.0)),
                // A pin marks its stop with its TIP, not its middle.
                ("icon-anchor".to_string(), json!("bottom")),
                // Grow the badge around whatever it has to say -- a stop
                // number, or "Depot". Without this the badge stays the size
                // its author drew it and anything longer than one digit
                // crowds or overflows it. The label then positions itself
                // in the box the badge grew, so no `text-offset` is needed.
                ("icon-text-fit".to_string(), json!("both")),
                ("icon-text-fit-padding".to_string(), json!(STOP_PIN_PADDING)),
                // Never drop a stop: collision is right for a basemap's
                // place names and wrong for features an application put on
                // the map on purpose. Without this, zooming out crowds the
                // pins and most of them vanish.
                //
                // The LABEL is deliberately NOT given the same treatment.
                // The reference style lets text overlap too, and the result
                // once pins stop being dropped is every number landing on a
                // neighbouring badge. Every stop keeps its pin; the numbers
                // appear wherever there is room for them.
                ("icon-allow-overlap".to_string(), json!(true)),
                ("icon-ignore-placement".to_string(), json!(true)),
            ]);
            if let Some(label) = properties.get("label") {
                pin.insert("text-field".to_string(), label.clone());
            }
            out.push(emit(pin));
        }
    }
    // Casings, then cores, then everything else (the stop pins), so the
    // pins stay on top of every route.
    let mut ordered = casings;
    ordered.extend(cores);
    ordered.extend(out);
    Ok(serde_json::to_string(&json!({"type": "FeatureCollection", "features": ordered}))?)
}

/// The name the stop pin is registered and referenced under.
const STOP_PIN_IMAGE: &str = "stop-pin";

/// A root view that puts an ordinary gpui panel OVER the map, to show that
/// `MapView::set_chrome_above_map` gives a real element -- one that paints,
/// highlights on hover, and takes clicks -- rather than merely a visible one.
///
/// Only rendered with `MAP_CHROME_DEMO=1`, so the example's default
/// appearance is unchanged.
struct Demo {
    map: gpui::Entity<MapView>,
    clicks: usize,
}

impl gpui::Render for Demo {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::prelude::*;

        let map = gpui::div().size_full().child(self.map.clone());
        if std::env::var("MAP_CHROME_DEMO").is_err() {
            return gpui::div().size_full().child(map);
        }
        gpui::div().relative().size_full().child(map).child(
            gpui::div()
                .id("chrome-demo")
                .absolute()
                // Clear of the titlebar: the root element spans the whole
                // window, traffic lights included.
                .top(px(80.0))
                .left(px(16.0))
                .w(px(300.0))
                .p(px(14.0))
                .rounded(px(10.0))
                .bg(gpui::rgb(0x111827))
                .text_color(gpui::rgb(0xF9FAFB))
                // Hover must visibly change the panel: the map underneath
                // also tracks the pointer, so "the cursor is over gpui, not
                // the map" is the thing being demonstrated.
                .hover(|style| style.bg(gpui::rgb(0x2563EB)))
                // Without this the map ALSO receives the click. gpui's
                // default `HitboxBehavior::Normal` does not block hitboxes
                // behind it, so `MapView`'s own `hitbox.is_hovered(window)`
                // is still true under this panel and its drag/click handlers
                // run -- clicking the panel pans the map. `occlude` is the
                // gpui-native way to say "the mouse stops here".
                .occlude()
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.clicks += 1;
                    cx.notify();
                }))
                .child(format!("gpui panel over the map — clicks: {}", self.clicks)),
        )
    }
}

/// `icon-text-fit-padding`, `[top, right, bottom, left]` in logical points.
///
/// Zero, which is both Mapbox's default and what the reference style this
/// is drawn against sets: its stop layer declares `icon-text-fit` and no
/// padding at all. An earlier value of 3pt left and right here was
/// invented, and it showed -- 6pt of extra width turned a two-digit badge
/// into a horizontal rectangle where the reference's is nearly square. The
/// sprite's own `content` box already carries the breathing room its author
/// wanted.
const STOP_PIN_PADDING: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

/// The pin's stretch bands and content box, as
/// `"x0,x1;x2,x3" / "y0,y1" / "l,t,r,b"` in IMAGE PIXELS -- Mapbox's
/// `stretchX`, `stretchY` and `content`, which is where these numbers come
/// from for any pin registered through `map.addImage`.
///
/// They belong to the image, not to this example, so they are read beside
/// it rather than hardcoded: a different pin has different bands.
fn stop_pin_geometry() -> Option<gpui_maps::sprite::IconGeometry> {
    fn bands(text: &str) -> Vec<(u32, u32)> {
        text.split(';')
            .filter(|band| !band.is_empty())
            .filter_map(|band| {
                let (start, end) = band.split_once(',')?;
                Some((start.trim().parse().ok()?, end.trim().parse().ok()?))
            })
            .collect()
    }
    let stretch_x = std::env::var("STOP_PIN_STRETCH_X").map(|v| bands(&v)).unwrap_or_default();
    let stretch_y = std::env::var("STOP_PIN_STRETCH_Y").map(|v| bands(&v)).unwrap_or_default();
    let content = std::env::var("STOP_PIN_CONTENT").ok().and_then(|v| {
        let parts: Vec<u32> = v.split(',').filter_map(|n| n.trim().parse().ok()).collect();
        <[u32; 4]>::try_from(parts).ok()
    });
    if stretch_x.is_empty() && stretch_y.is_empty() && content.is_none() {
        return None;
    }
    Some(gpui_maps::sprite::IconGeometry { stretch_x, stretch_y, content })
}

/// The stop pin and how many of its pixels make one logical point.
///
/// `STOP_PIN_PNG` names an SDF pin to load -- an alpha-as-distance image, of
/// the kind Mapbox's own tooling emits and `map.addImage(..., {sdf: true})`
/// takes. Point it at the pin your production map registers and this draws
/// that exact icon, recoloured per route by `icon-color`/`icon-halo-color`.
/// `STOP_PIN_PIXEL_RATIO` matches the `pixelRatio` it is registered with
/// (Mapbox's default is 1; a pin authored at 1.5x needs 1.5, or it draws
/// half again too large).
///
/// Kept as a PATH rather than a checked-in asset on purpose: this repository
/// is public, and a pin taken from a private product does not belong in it.
///
/// With no path set, falls back to a generated disc so the example runs
/// standalone.
fn stop_pin_image() -> (image::RgbaImage, f32) {
    let ratio = std::env::var("STOP_PIN_PIXEL_RATIO")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(DEFAULT_STOP_PIN_PIXEL_RATIO);
    match std::env::var("STOP_PIN_PNG") {
        Ok(path) => match image::open(&path) {
            // `to_rgba8` regardless of what the file was: a Mapbox SDF pin
            // is commonly an indexed PNG with a `tRNS` table, and the
            // distance field lives in the ALPHA that expands to.
            Ok(image) => (image.to_rgba8(), ratio),
            Err(err) => {
                eprintln!("stop pin {path:?} unavailable, using the generated disc: {err}");
                (generated_stop_pin(), DEFAULT_STOP_PIN_PIXEL_RATIO)
            }
        },
        Err(_) => (generated_stop_pin(), DEFAULT_STOP_PIN_PIXEL_RATIO),
    }
}

/// The generated fallback's pixel ratio: 40 image pixels over a 20-point
/// badge.
const DEFAULT_STOP_PIN_PIXEL_RATIO: f32 = 2.0;

/// A stop pin, as a signed distance field: a filled disc with room around
/// it for a border.
///
/// Built here rather than shipped as a PNG because an SDF is not an ordinary
/// image -- its alpha is distance, not opacity -- and `sdf_from_mask` is
/// what turns a shape into that encoding.
///
/// 40 pixels at `pixel_ratio` 2 is a 20-point badge. The disc's radius
/// leaves 6 pixels of margin, which is exactly the room the encoding gives a
/// halo: a border wider than that would be clipped by the sprite's own edge
/// rather than by the field.
fn generated_stop_pin() -> image::RgbaImage {
    const SIZE: u32 = 40;
    const RADIUS: f32 = 13.0;
    let centre = (SIZE as f32 - 1.0) / 2.0;
    let mask = image::GrayImage::from_fn(SIZE, SIZE, |x, y| {
        let dx = x as f32 - centre;
        let dy = y as f32 - centre;
        image::Luma([if (dx * dx + dy * dy).sqrt() <= RADIUS { 255 } else { 0 }])
    });
    gpui_maps::sprite::sdf_from_mask(&mask)
}

/// Where most of the data is, so the camera opens on it.
///
/// The MEDIAN position, not the bounding-box centre. This dataset spans
/// 9-17 degrees of latitude while its routes cluster around one city, so
/// the bbox centre lands in the Gulf of Thailand with nothing on screen.
/// A median ignores the outliers that produce that.
fn centre_of(raw: &str) -> Option<(f64, f64)> {
    let source: Value = serde_json::from_str(raw).ok()?;
    let (mut lons, mut lats) = (Vec::new(), Vec::new());
    for feature in source["features"].as_array()? {
        let geometry = &feature["geometry"];
        let Some(coordinates) = geometry.get("coordinates") else { continue };
        // One position per feature is enough for a median and keeps a
        // 400k-vertex document from being walked twice.
        let position = match geometry["type"].as_str() {
            Some("Point") => coordinates,
            _ => &coordinates.as_array()?[0],
        };
        if let (Some(lon), Some(lat)) = (position[0].as_f64(), position[1].as_f64()) {
            lons.push(lon);
            lats.push(lat);
        }
    }
    if lons.is_empty() {
        return None;
    }
    lons.sort_by(f64::total_cmp);
    lats.sort_by(f64::total_cmp);
    Some((lons[lons.len() / 2], lats[lats.len() / 2]))
}

/// Blocks on `sprite::fetch_sprite_atlas`, which exists only with the
/// `sprite_http_fetch` feature (a default one). A `--no-default-features`
/// build gets an error here and draws without icons.
fn fetch_sprites_blocking(
    handle: &tokio::runtime::Handle,
    url: &str,
) -> anyhow::Result<gpui_maps::sprite::SpriteAtlas> {
    #[cfg(feature = "sprite_http_fetch")]
    return handle.block_on(gpui_maps::sprite::fetch_sprite_atlas(url, &reqwest::Client::new()));
    #[cfg(not(feature = "sprite_http_fetch"))]
    {
        let _ = (handle, url);
        Err(anyhow::anyhow!("built without the sprite_http_fetch feature"))
    }
}

/// `MAPBOX_ACCESS_TOKEN`, gated: `None` unless `GPUI_MAP_ALLOW_MAPBOX=1`.
/// Mapbox is off by default (see `kit/src/kit.rs::MAPBOX_ALLOW_VAR`,
/// the same switch). Never printed.
fn gated_mapbox_token() -> Option<String> {
    if std::env::var("GPUI_MAP_ALLOW_MAPBOX").ok().as_deref().map(str::trim) != Some("1") {
        return None;
    }
    std::env::var("MAPBOX_ACCESS_TOKEN").ok().filter(|t| !t.trim().is_empty())
}

fn main() -> anyhow::Result<()> {
    let path = std::env::var("SIMULATION_GEOJSON").map_err(|_| {
        anyhow::anyhow!("set SIMULATION_GEOJSON to the path of a GeoJSON FeatureCollection")
    })?;
    let raw = std::fs::read_to_string(&path)?;
    println!("{path}: {:.1} MB", raw.len() as f64 / 1_048_576.0);

    let started = std::time::Instant::now();
    let features = translate(&raw)?;
    println!("translated in {:?}", started.elapsed());
    let (centre_lon, centre_lat) = centre_of(&raw).unwrap_or((103.85, 1.29));
    println!("centred on {centre_lon:.4}, {centre_lat:.4}");

    let runtime = tokio::runtime::Runtime::new()?;
    let handle = runtime.handle().clone();

    // A Mapbox style if one is named, otherwise the vendored Liberty style.
    // The point of allowing the former is that comparing our overlay against
    // a production mapbox-gl map is only meaningful over the SAME basemap:
    // Liberty draws saturated orange roads and green landuse that a light
    // fleet-tracking style deliberately suppresses, and against that
    // background coloured routes compete with the map instead of sitting on
    // it. Fetch one with
    //   curl -o style.json \
    //     "https://api.mapbox.com/styles/v1/<user>/<style-id>?access_token=$MAPBOX_ACCESS_TOKEN"
    let mapbox_style = std::env::var("STYLE_JSON").ok();
    let style_bytes = match &mapbox_style {
        Some(path) => std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("STYLE_JSON={path}: {e}"))?,
        None => std::fs::read("examples/liberty-style.json")
            .map_err(|e| anyhow::anyhow!("run from the repo root: {e}"))?,
    };
    let style = gpui_maps::style_spec::parse_and_compile(&style_bytes)?;

    // A Mapbox style names its own tilesets; derive them from the document
    // rather than hardcoding them, so this cannot drift from whatever style
    // is passed in.
    let style_json: serde_json::Value = serde_json::from_slice(&style_bytes)?;
    let mapbox = match &mapbox_style {
        None => None,
        Some(_) => {
            let token = gated_mapbox_token().ok_or_else(|| {
                anyhow::anyhow!("a Mapbox style needs GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN set")
            })?;
            let tilesets = style_json["sources"]["composite"]["url"]
                .as_str()
                .and_then(|url| url.strip_prefix("mapbox://"))
                .ok_or_else(|| anyhow::anyhow!("expected sources.composite.url"))?
                .to_string();
            Some((token, tilesets))
        }
    };

    // Basemap icons, from whichever sheet the loaded style names -- a
    // Mapbox style's `mapbox://sprites/...` (which needs the token) or
    // Liberty's plain HTTPS base (which does not); `resolve_sprite_url`
    // tells the two apart. Fetched before the window opens, and a failure
    // only costs icons -- the draw loop skips them and still draws text.
    let sprites = style.sprite.as_deref().and_then(|sprite| {
        let token = mapbox.as_ref().map(|(token, _)| token.as_str());
        match gpui_maps::sprite::resolve_sprite_url(sprite, token)
            .and_then(|url| fetch_sprites_blocking(&handle, &url))
        {
            Ok(atlas) => Some(Arc::new(atlas)),
            Err(err) => {
                eprintln!("sprite atlas unavailable, icons will not render: {err:#}");
                None
            }
        }
    });
    println!(
        "basemap: {}",
        if mapbox.is_some() { "Mapbox (style supplied)" } else { "OpenFreeMap Liberty" }
    );

    gpui_platform::application().run(move |cx: &mut App| {
        let source: Arc<dyn TileSource> = match &mapbox {
            // INCOMPLETE for a shipping product: Mapbox's terms require the
            // Mapbox wordmark image alongside this text, and gpui-maps draws
            // text only.
            Some((token, tilesets)) => Arc::new(HttpTileSource::new(
                format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}"),
                16,
                concat!(
                    r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
                    r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a> "#,
                    r#"<a href="https://www.mapbox.com/contribute/" target="_blank">Improve this map</a>"#,
                ),
            )),
            None => Arc::new(HttpTileSource::new(
                "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf",
                14,
                concat!(
                    r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
                    r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
                    r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
                ),
            )),
        };
        let handle = handle.clone();
        let style = style.clone();
        let features = features.clone();
        let sprites = sprites.clone();

        let bounds = Bounds::centered(None, size(px(1280.0), px(900.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Simulation Overlay".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |_window, cx| {
                let view = cx.new(|_cx| {
                    let mut view = MapView::new(
                        centre_lon, centre_lat, OPENING_ZOOM, 1280.0, 900.0, style, source, handle,
                    );
                    if let Some(sprites) = sprites {
                        view.set_sprites(sprites);
                    }
                    // Places the map below gpui's own layer so `Demo`'s panel
                    // is a real element on top of it rather than a hidden one.
                    if std::env::var("MAP_CHROME_DEMO").is_ok() {
                        view.set_chrome_above_map(true);
                    }
                    view
                });
                view.update(cx, |view, cx| {
                    // Registered here rather than baked into the fetched
                    // sheet: neither OpenFreeMap's nor Mapbox's sprite sheet
                    // contains a single SDF sprite, so a caller who wants
                    // one supplies it. `add_image` ADDS to whatever atlas is
                    // already set, so the basemap keeps its own icons.
                    let (pin, pin_ratio) = stop_pin_image();
                    view.add_image(STOP_PIN_IMAGE, pin, pin_ratio, true);
                    if let Some(geometry) = stop_pin_geometry() {
                        view.set_image_geometry(STOP_PIN_IMAGE, geometry);
                    }
                    let started = std::time::Instant::now();
                    match view.set_features(&features, cx) {
                        Ok(()) => println!("set_features accepted in {:?}", started.elapsed()),
                        Err(error) => eprintln!("set_features rejected: {error}"),
                    }
                });
                cx.new(|_cx| Demo { map: view, clicks: 0 })
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
    Ok(())
}

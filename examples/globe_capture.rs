//! **Renders a named camera headlessly and writes the frame to a PNG.**
//! No window, no focus stolen, no `gpui`.
//!
//! ```text
//! cargo run --release --example globe_capture -- \
//!     --lon 88.8 --lat 61 --zoom 3.14 --size 1024 \
//!     --subdivision for-tile --out /tmp/z314-fortile.png
//! ```
//!
//! Run it twice with `--subdivision for-tile` and `--subdivision none` and
//! it prints the difference between the two frames itself — that is the A/B
//! this harness was built for. `--ab` does both halves in one process,
//! which is what the reported camera was graded with.
//!
//! Flags, all optional:
//!
//! | flag | default | meaning |
//! |---|---|---|
//! | `--lon`, `--lat`, `--zoom` | 88.8, 61, 3.14 | the camera |
//! | `--pitch` | 0 | camera tilt, degrees |
//! | `--bearing` | 0 | camera rotation, degrees clockwise from north |
//! | `--projection` | none | `equal-earth` or `robinson`: a flat world projection drawn from the same Mercator tiles (spike) |
//! | `--plain` | off | Plain: the globe switched off (flat Mercator at every zoom), the camera held inside the world as `MapView` holds it (`camera_control::plain_constrain`) |
//! | `--unconstrained` | off | with `--plain`: draw the camera as given, past the world's edge (the before picture) |
//! | `--flat` | off | draw a street-level flat frame through `cover_plan::flat_cover`, with the horizon fade |
//! | `--size` | 1024 | square frame, physical pixels; `WxH` for any other shape |
//! | `--pixel-ratio` | 1 | physical pixels per logical pixel: `--size 3200x2000 --pixel-ratio 2` is the 1600x1000 view drawn for a Retina screen |
//! | `--subdivision` | `for-tile` | `for-tile` or `none` |
//! | `--clear` | `style` | `style`, `sentinel` (opaque magenta) or `black` (for `--paint wash`) |
//! | `--paint` | `as-styled` | `as-styled`, `flat` (fills only, opaque) or `wash` (fills at opacity 0.3, so the pixel value counts composites) |
//! | `--absent N` | 0 | treat N cover tiles as still loading, drawing their ancestor under the ready ones |
//! | `--absent-head N` | 0 | the same, but the N MOST visible tiles — where a hole cannot hide |
//! | `--no-stand-in` | off | withhold those ancestors too, so the missing tiles are simply not drawn — the control for what a stand-in is worth |
//! | `--lit` | off | light the ground with the terminator through the middle of the disc |
//! | `--sun LON,LAT` | none | light the ground AND the sky from this subsolar point |
//! | `--stars` | off | draw the star field behind the sky, as the shells do |
//! | `--space-look` | off | the opt-in space look (star map, clouds, sun shading, rim) from `~/.cache/gpui-map/space/` |
//! | `--look-time UNIX` | none | turn the space look's sky and stars to that instant's sidereal angle |
//! | `--time N` | off | with `--space-look`: no PNG; render the frame N times per arm, look off and on interleaved, in one session, and print each arm's wall ms p50/p95/max (encode + GPU + readback) |
//! | `--no-backdrop` | off | drop the ocean sphere, so an uncovered pixel is the clear colour |
//! | `--extrusion-opacity` | the style's own | override `fill-extrusion-opacity` for every extruded layer (0..1) |
//! | `--extrusion-look` | `maplibre` | `enhanced`: `ExtrusionLook::ENHANCED` shading (contrast, occlusion, tone, roof tint and edge, haze) |
//! | `--label-occlusion` | off | hide point labels a building hides from the eye |
//! | `--no-building-floor` | floor on with buildings | let the cover drop below the building data's z13 (the control for the far-edge fix) |
//! | `--extrusion-flat` | off | draw every building at height 0: the control a wall check diffs against |
//! | `--buildings-3d` | off | opt the style into 3D buildings (`style_spec::with_3d_buildings`) before compiling, e.g. Mapbox Streets |
//! | `--atmosphere` | off | draw the limb glow over the tiles, as `MapView::paint` does |
//! | `--engine-default UNIX` | off | a fresh `MapView`'s sun, terminator, stars and glow at that instant (overrides `--sun`/`--atmosphere`/`--stars`) |
//! | `--labels` | off | place and draw the POINT labels over the tiles, through `symbol_pass` |
//! | `--label-fonts` | system | `FAMILY=FILE,FILE,...`: shape labels with only these faces, as the web/iOS shells do (e.g. `Noto Sans JP=assets/fonts/NotoSansJP-Regular.japan-subset.otf,...`) |
//! | `--nav-route` | none | an OSRM route file: draw a paused simulated drive's route line and chrome, from its settled follow camera (replaces `--lon`..`--bearing`; use with `--flat`) |
//! | `--nav-at` | 0 | metres along `--nav-route` to place the puck |
//! | `--nav-camera` | `follow3d` | the navigation camera mode: `follow3d`, `follow2d`, `northup2d` or `overview` |
//! | `--nav-proximity` | `standard` | the follow zoom offset as a preset: `close` (+1), `standard` (0) or `far` (-1) |
//! | `--nav-zoom-offset` | 0 | the follow zoom offset in zoom levels (+1 is a level closer) |
//! | `--nav-zoom-by` | none | mid-drive, once the follow camera has settled, a wheel zoom of this many levels (the camera keeps following and takes it as its offset), and capture 1.2 s later |
//! | `--nav-own-camera` | off | keep `--lon`..`--bearing` instead of the follow camera, e.g. to see the route and puck on the globe |
//! | `--synthetic-shells` | off | draw the altitude layer's §5.3 scene: a GEO ring of 72, a LEO ring of 36 at 550 km, and a static 2,000-dot Walker shell (53°, 550 km), in the §1.2 group colours |
//! | `--satellites FILE` | none | (`--features orbits`) an OMM JSON file: every object propagated to `--at` and drawn above the globe, painted by group |
//! | `--at UTC` | the median epoch | (`--features orbits`) the scene time for `--satellites`, RFC 3339 (e.g. `2026-09-28T11:09:16Z`) |
//! | `--select NORAD` | none | (`--features orbits`, with `--satellites`) that object's orbit ring (inertial, 1.5 px, group colour at 70%), its ground track as an overlay line, and a white halo on its dot |
//! | `--groups DIR` | none (all grey `Other` but Starlink) | (`--features orbits`, with `--satellites`) classify by the CelesTrak group lists `DIR/{stations,gps-ops,geo}.json`, each one that is missing left unclassified |
//! | `--halo NORAD` | the `--select` object | (`--features orbits`, with `--satellites`) the object that carries the white halo |
//! | `--dim-others` | off | (`--features orbits`, with `--select` or `--halo`) paint every group but the emphasised object's at 60% alpha, so it reads first |
//! | `--overlay` | none | a GeoJSON FeatureCollection file, drawn over the tiles as a `MapView` overlay source (e.g. `tests/fixtures/overlay_us_synthetic.geojson`) |
//! | `--overlay-paint` | none | a JSON file of the `--overlay`'s paint (`circle-color`, `fill-color`, ...; expressions as `MapView` takes them) |
//! | `--out` | none | where to write the PNG |
//! | `--ab` | off | render BOTH subdivisions and difference them |
//! | `--cache` | a scratch dir, per source | where fetched tile bytes are kept |
//! | `--style` | `examples/liberty-style.json` | the style document to compile |
//! | `--source` | `openfreemap` | `openfreemap` or `mapbox` (needs `MAPBOX_ACCESS_TOKEN`) |
//! | `--raster` | none | imagery basemap: `natural-earth` (public domain), `s2cloudless` (the 2016 layer, CC BY 4.0), or `mapbox-satellite` (licensed, needs `MAPBOX_ACCESS_TOKEN`, NOT committable) |
//! | `--raster-opacity` | the style's own | override `raster-opacity`, for grading the property itself |
//! | `--layer-off` | none | comma-separated style layer ids to drop before baking |
//! | `--only-layer` | none | comma-separated style layer ids to KEEP — everything else is dropped |
//! | `--only-tile` | none | `z/x/y`: draw just that one tile of the cover |
//! | `--explain-tile` | none | `z/x/y`: print where that tile's `[0,1]^2` lands in pixels, no GPU, no tiles |
//! | `--explain-cover` | off | print the display level's `cover` vs `tile_visible` sets, no GPU, no tiles |
//!
//! # Rendering the USER's map, not a stand-in for it
//!
//! The defaults above draw OpenFreeMap tiles with the vendored Liberty
//! style, and that is a different map from the one the report is about.
//! Liberty's fills are opaque almost everywhere; the user runs
//! `examples/mapbox-streets-style.json` over the Mapbox `composite` source,
//! whose `landcover` is a near-white wash at `fill-opacity` 0.4 and whose
//! `snow` class covers exactly the latitudes the streaks were reported at.
//! A frame captured on Liberty therefore cannot contain the symptom, and
//! six investigations graded one that could not.
//!
//! ```text
//! MAPBOX_ACCESS_TOKEN=pk... cargo run --release --example globe_capture -- \
//!     --source mapbox --style examples/mapbox-streets-style.json \
//!     --lon 88.8 --lat 61 --zoom 3.14 --lit --out /tmp/streets.png
//! ```
//!
//! The fetched Mapbox bytes are licensed data: they are cached OUTSIDE the
//! repository, under a directory of their own so they can never be read
//! back as the OpenFreeMap tiles they share `z-x-y.pbf` names with, and
//! they are never committed. See `src/capture.rs`' module docs.
//!
//! # The tiles are OpenFreeMap's, and they stay out of the repository
//!
//! Same source `examples/basic_map.rs` draws: OpenFreeMap's keyless planet
//! tiles, built from OpenStreetMap (ODbL). The fetched bytes are cached
//! under `--cache`, which defaults to a directory in the system temp area —
//! **not** under the repo. Do not repoint this at Mapbox and commit what
//! comes back; see `src/capture.rs`' module docs.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_maps::capture::{
    capture, fetch_cover, Capture, CaptureRequest, Clear, Paint, SubdivisionPolicy,
};
use gpui_maps::tile_source::http::HttpTileSource;
use gpui_maps::tile_source::TileSource;

/// The exact OpenFreeMap path `examples/basic_map.rs` uses, and for the
/// reasons its comment gives.
const OPENFREEMAP: &str = "https://tiles.openfreemap.org/planet/20260802_080001_pt/{z}/{x}/{y}.pbf";
const ATTRIBUTION: &str = concat!(
    r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> "#,
    r#"<a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> "#,
    r#"Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#,
);

const MAPBOX_ATTRIBUTION: &str = concat!(
    r#"<a href="https://www.mapbox.com/about/maps" target="_blank">&copy; Mapbox</a> "#,
    r#"<a href="https://www.openstreetmap.org/copyright/" target="_blank">&copy; OpenStreetMap</a>"#,
);

/// The Mapbox `composite` source `examples/mapbox_standard_dark.rs` builds,
/// assembled the same way: the tileset ids come out of the streets style's
/// own `sources.composite.url`, comma-joined per Mapbox's raw tile API, and
/// the token comes from the environment.
///
/// The token is read here and never printed: the returned url is only ever
/// handed to `HttpTileSource`, and this harness prints the CACHE path, not
/// the url.
fn mapbox_source() -> anyhow::Result<Arc<dyn TileSource>> {
    let style_bytes = std::fs::read("examples/mapbox-streets-style.json")
        .map_err(|e| anyhow::anyhow!("run from the repo root: {e}"))?;
    let json: serde_json::Value = serde_json::from_slice(&style_bytes)?;
    let tilesets = json["sources"]["composite"]["url"]
        .as_str()
        .and_then(|u| u.strip_prefix("mapbox://"))
        .ok_or_else(|| anyhow::anyhow!("sources.composite.url must be a mapbox:// url"))?;
    let token = gated_mapbox_token().ok_or_else(|| {
        anyhow::anyhow!("--source mapbox needs GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN (a public `pk.` token)")
    })?;
    let url =
        format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}");
    Ok(Arc::new(HttpTileSource::new(url, 16, MAPBOX_ATTRIBUTION)))
}

/// Natural Earth II shaded relief, served by OpenFreeMap. The very source
/// `examples/liberty-style.json`'s `ne2_shaded` names, so `--raster
/// natural-earth` draws the layer the vendored style has been declaring
/// and this engine has been dropping.
///
/// **Natural Earth is public domain**, so a frame rendered from it is
/// committable evidence. 512x512 PNG, capped at z6 by the tileset.
const NATURAL_EARTH: &str = "https://tiles.openfreemap.org/natural_earth/ne2sr/{z}/{x}/{y}.png";
const NATURAL_EARTH_ATTRIBUTION: &str =
    r#"<a href="https://www.naturalearthdata.com/" target="_blank">Natural Earth</a>"#;

/// Sentinel-2 cloudless by EOX, the **2016** layer, **CC BY 4.0** (later layers are CC BY-NC-SA and not used) -- the satellite basemap
/// this work exists for, and an openly-licensed one.
///
/// Note `{z}/{y}/{x}`, not `{z}/{x}/{y}`: this is a WMTS endpoint and WMTS
/// orders its path row-then-column. `HttpTileSource::url_for` substitutes
/// the three names wherever they appear, so the template expresses it
/// directly and no y-flip or reorder is needed. 256x256 JPEG.
const S2_CLOUDLESS: &str =
    "https://tiles.maps.eox.at/wmts/1.0.0/s2cloudless_3857/default/g/{z}/{y}/{x}.jpg";
const S2_CLOUDLESS_ATTRIBUTION: &str = concat!(
    r#"<a href="https://cloudless.eox.at" target="_blank">EOxCloudless https://cloudless.eox.at</a> by EOX IT Services GmbH "#,
    r#"(Contains modified Copernicus Sentinel data 2016 & 2017)"#,
);

/// The raster source `--raster` names, its tile size, and its own cache
/// directory suffix.
///
/// The suffix matters for the same licensing reason the vector cache is
/// keyed on its source name: two sources' tiles must never share a
/// `z-x-y` filename, or one source's bytes get read back as the other's.
fn raster_source(name: &str) -> anyhow::Result<(Arc<dyn TileSource>, u32, &'static str)> {
    match name {
        "natural-earth" => Ok((
            Arc::new(HttpTileSource::new(NATURAL_EARTH, 6, NATURAL_EARTH_ATTRIBUTION)),
            512,
            "natural-earth",
        )),
        "s2cloudless" => Ok((
            Arc::new(HttpTileSource::new(S2_CLOUDLESS, 14, S2_CLOUDLESS_ATTRIBUTION)),
            256,
            "s2cloudless-2016",
        )),
        "mapbox-satellite" => {
            // Eyeballing only. Mapbox imagery is licensed and a frame
            // rendered from it is NOT committable evidence -- which is why
            // the two openly-licensed sources above exist and this one has
            // to be asked for by name.
            //
            // The token is read here and never printed: it goes straight
            // into `HttpTileSource`, and this harness prints the cache
            // path, never the url.
            let token = gated_mapbox_token().ok_or_else(|| {
                anyhow::anyhow!(
                    "--raster mapbox-satellite needs GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN (a public `pk.` token)"
                )
            })?;
            let url = format!(
                "https://api.mapbox.com/v4/mapbox.satellite/{{z}}/{{x}}/{{y}}.jpg?access_token={token}"
            );
            Ok((Arc::new(HttpTileSource::new(url, 22, MAPBOX_ATTRIBUTION)), 256, "mapbox-satellite"))
        }
        other => anyhow::bail!(
            "--raster takes `natural-earth`, `s2cloudless` or `mapbox-satellite`, got `{other}`"
        ),
    }
}

/// Fetches and decodes the raster cover, read-through a disk cache of its
/// own.
///
/// A separate function from `capture::fetch_cover` rather than a parameter
/// on it, because the two differ in every particular: a different
/// extension, a different decoder, a different meaning for a decode
/// failure. Sharing them would mean threading a format enum through a
/// function whose whole body is four lines.
fn fetch_raster_cover(
    runtime: &tokio::runtime::Runtime,
    source: &dyn TileSource,
    tiles: &[gpui_maps::geo::TileId],
    tile_size: u32,
    cache_dir: &std::path::Path,
) -> anyhow::Result<std::collections::HashMap<gpui_maps::geo::TileId, image::RgbaImage>> {
    std::fs::create_dir_all(cache_dir)?;
    let mut out = std::collections::HashMap::new();
    for &tile in tiles {
        // `.img`, not `.png` or `.jpg`: the extension records that these
        // are image bytes whose format is sniffed at decode, which is what
        // `raster::decode` does. Claiming a format in the filename would
        // be a claim this cache cannot check.
        let path = cache_dir.join(format!("{}-{}-{}.img", tile.z, tile.x, tile.y));
        let bytes = if path.exists() {
            std::fs::read(&path)?
        } else {
            let Some(bytes) = runtime.block_on(source.fetch(tile))? else { continue };
            std::fs::write(&path, &bytes)?;
            bytes
        };
        match gpui_maps::raster::decode_tile_at(&bytes, tile_size, tile) {
            Ok(image) => {
                out.insert(tile, image);
            }
            // Loud, and naming the TILE rather than the url -- a raster
            // tile that will not decode is worth reporting, and a url can
            // carry a token.
            Err(e) => eprintln!("raster tile {}/{}/{} did not decode: {e}", tile.z, tile.x, tile.y),
        }
    }
    Ok(out)
}

/// `--arcs FILE`: one arc layer (`render::arcs`) from JSON,
/// `{"blend": "normal"|"additive"|"screen"|"glow", "height": 1,
/// "max_segments": 50, "opacity": 1, "exposure": 1, "desaturate": 0.35,
/// "arcs": [[lon0, lat0, lon1, lat1, rgba8_le, width_px], ...]}` -- what an
/// arc or flat-line layer draws, blend and widths included (the Explorer's
/// Glow flows export it). Every key but `arcs` is optional, with
/// `ArcStyle`'s defaults.
fn arc_layer(path: &str) -> anyhow::Result<gpui_maps::capture::CaptureArcs> {
    use gpui_maps::render::arcs::{ArcBlend, ArcInstance, ArcSet, ArcStyle};
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).map_err(|e| anyhow::anyhow!("--arcs {path}: {e}"))?)?;
    let d = ArcStyle::default();
    let num = |k: &str, or: f64| v.get(k).and_then(serde_json::Value::as_f64).unwrap_or(or);
    let blend = match v.get("blend").and_then(serde_json::Value::as_str).unwrap_or("normal") {
        "additive" => ArcBlend::Additive,
        "screen" => ArcBlend::Screen,
        "glow" => ArcBlend::Glow,
        "normal" => ArcBlend::Normal,
        other => anyhow::bail!("--arcs {path}: unknown blend `{other}`"),
    };
    let style = ArcStyle {
        opacity: num("opacity", f64::from(d.opacity)) as f32,
        height: num("height", f64::from(d.height)) as f32,
        blend,
        max_segments: num("max_segments", f64::from(d.max_segments)) as u32,
        exposure: num("exposure", f64::from(d.exposure)) as f32,
        desaturate: num("desaturate", f64::from(d.desaturate)) as f32,
    };
    let arcs = v.get("arcs").and_then(serde_json::Value::as_array).ok_or_else(|| anyhow::anyhow!("--arcs {path}: no `arcs` array"))?;
    let mut instances = Vec::with_capacity(arcs.len());
    for a in arcs {
        let f: Vec<f64> = a.as_array().map(|x| x.iter().filter_map(serde_json::Value::as_f64).collect()).unwrap_or_default();
        anyhow::ensure!(f.len() == 6, "--arcs {path}: an arc is [lon0, lat0, lon1, lat1, rgba8, width], got {a}");
        let color = f[4] as u32;
        instances.push(ArcInstance { source: [f[0] as f32, f[1] as f32], target: [f[2] as f32, f[3] as f32], source_color: color, target_color: color, width_px: f[5] as f32, filter: f32::NAN });
    }
    eprintln!("arcs: {} from {path}, {blend:?}, height {}, max {} segments", instances.len(), style.height, style.max_segments);
    Ok(gpui_maps::capture::CaptureArcs { set: std::sync::Arc::new(ArcSet::new(instances)), style, window: None })
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Parses a `z/x/y` tile spec.
fn parse_tile(one: &str, flag_name: &str) -> anyhow::Result<gpui_maps::geo::TileId> {
    let parts: Vec<&str> = one.split('/').collect();
    anyhow::ensure!(parts.len() == 3, "{flag_name} takes z/x/y, got `{one}`");
    Ok(gpui_maps::geo::TileId { z: parts[0].parse()?, x: parts[1].parse()?, y: parts[2].parse()? })
}

/// **`--explain-cover`: every tile of the display level, scored by the
/// cover's own two gates, with NO GPU and no tile bytes.**
///
/// A frame that shows a lens -- a wedge of planet with no tiles on it --
/// has two possible causes and they need opposite fixes:
///
///  - **the cover stopped short**: `GlobeCover::cover`'s depth-first
///    descent never reached the tile, so nothing was ever fetched for it;
///    or
///  - **the geometry was clipped after it**: the tile IS in the cover and
///    was drawn, and something downstream (the far-side clip, the
///    subdivision, a stand-in's `TileClip`) removed it.
///
/// `cover` and `tile_visible` are the two halves that separate those. A
/// tile that `tile_visible` accepts but `cover` does not return is the
/// first case, and the descent is what to look at. A tile in the cover
/// that is still missing from the frame is the second.
///
/// The per-tile verdict is broken into the two gates `visibility` applies
/// in order, so a rejection names which one did it:
///
///  - the **horizon** gate, `max_dot_over_tile(tile) + plane_w < 0`, an
///    analytic maximum over the tile's whole lon/lat rectangle (extended
///    to the pole for a pole row) -- "keep if ANY part of the tile is in
///    front", not a centre sample; and
///  - the **frustum** gate, the tile's bounding ball against the frame's
///    planes, whose ball is built from the minimum corner-to-centre dot.
fn explain_cover(request: &CaptureRequest) -> anyhow::Result<()> {
    use gpui_maps::render::globe::{globe_frame, GlobeCover};

    let camera = request.camera();
    let frame = globe_frame(&camera);
    let cover_tiles = request.cover();
    anyhow::ensure!(
        !cover_tiles.is_empty(),
        "this camera has no globe cover; zoom {} is outside the globe band",
        request.zoom
    );
    let zoom = cover_tiles[0].z;
    let cover = GlobeCover::new(&frame);
    anyhow::ensure!(cover.is_globe(), "the globe is off at zoom {}", request.zoom);

    println!("# --explain-cover");
    println!(
        "camera lon {} lat {} zoom {} -> display zoom {zoom}, transition {:.5}",
        request.center_lon, request.center_lat, request.zoom, frame.transition
    );
    let side = 1u32 << zoom;
    let mut visible = Vec::new();
    for y in 0..side {
        for x in 0..side {
            let tile = gpui_maps::geo::TileId { z: zoom, x, y };
            if cover.tile_visible(tile) {
                visible.push(tile);
            }
        }
    }
    let in_cover: std::collections::HashSet<_> = cover_tiles.iter().copied().collect();
    let visible_set: std::collections::HashSet<_> = visible.iter().copied().collect();
    // The whole point of the dump: the two sets, and their difference.
    let kept_not_covered: Vec<_> =
        visible.iter().copied().filter(|t| !in_cover.contains(t)).collect();
    let covered_not_kept: Vec<_> =
        cover_tiles.iter().copied().filter(|t| !visible_set.contains(t)).collect();
    println!(
        "level {zoom}: {} tiles total, tile_visible keeps {}, cover returns {}",
        side * side,
        visible.len(),
        cover_tiles.len()
    );
    println!(
        "  tile_visible KEEPS but cover DOES NOT RETURN: {} {:?}",
        kept_not_covered.len(),
        kept_not_covered.iter().map(|t| format!("{}/{}/{}", t.z, t.x, t.y)).collect::<Vec<_>>()
    );
    println!(
        "  cover RETURNS but tile_visible REJECTS: {} {:?}",
        covered_not_kept.len(),
        covered_not_kept.iter().map(|t| format!("{}/{}/{}", t.z, t.x, t.y)).collect::<Vec<_>>()
    );
    // A map of the level, so a lens is a SHAPE rather than a list:
    // `#` in both, `v` kept by tile_visible only, `c` in the cover only,
    // `.` in neither.
    println!("map (rows = y, cols = x): # both, v tile_visible only, c cover only, . neither");
    for y in 0..side {
        let row: String = (0..side)
            .map(|x| {
                let tile = gpui_maps::geo::TileId { z: zoom, x, y };
                match (visible_set.contains(&tile), in_cover.contains(&tile)) {
                    (true, true) => '#',
                    (true, false) => 'v',
                    (false, true) => 'c',
                    (false, false) => '.',
                }
            })
            .collect();
        println!("  y={y:<3} {row}");
    }
    Ok(())
}

/// **`--explain-tile z/x/y`: where one tile's `[0,1]^2` actually lands on
/// screen, with NO GPU and no tile bytes.**
///
/// A frame can show a tile's western quarter collapsing into a fan of
/// slivers and still not say whether the fan is in the GEOMETRY or in the
/// DATA: `--paint flat` draws fills, and a fill is only where the style
/// painted one, so the tile's own footprint is never directly visible. This
/// walks the tile's parameter square instead, through the SAME CPU twins
/// the shader's arithmetic is mirrored by (`render::globe::TileAnchors`,
/// whose `sphere` reaches `mercator_to_sphere` and whose `project` is
/// `interpolate_projection` minus the far-side z), and prints where each
/// grid point lands in pixels.
///
/// What it is for: a projection that collapses a region maps a whole band
/// of the parameter square onto one point, so the grid CELLS there have
/// near-zero screen area while their neighbours do not. The summary at the
/// end reports exactly that, per quarter of the tile in longitude, so
/// "the WESTERN quarter collapses" is a number rather than a reading of a
/// picture.
fn explain_tile(request: &CaptureRequest, tile: gpui_maps::geo::TileId) -> anyhow::Result<()> {
    use gpui_maps::render::globe::{globe_frame, GlobeAnchors};

    let camera = request.camera();
    let frame = globe_frame(&camera);
    let anchors = GlobeAnchors::new(&frame);
    let extent = f64::from(gpui_maps::mvt::MVT_EXTENT);
    let per_tile = anchors
        .for_tile(tile, extent)
        .ok_or_else(|| anyhow::anyhow!("the globe is off at zoom {}", request.zoom))?;
    // The flat half of the blend, in the LYON units the vertex buffer
    // carries: `tile_model_matrix_f64` scales by `1 / (1000 * extent)`, so
    // a `[0,1]` in-tile coordinate is `u * 1000 * extent` here. Same matrix
    // the draw uses, from the same camera.
    let matrix =
        gpui_maps::render::combined_tile_matrix(&camera, tile, gpui_maps::mvt::MVT_EXTENT as f32);
    let lyon_per_unit = 1000.0 * extent;

    let (w, h) = (f64::from(request.width), f64::from(request.height));
    // Screen pixels for one `[0,1]^2` point of the tile, or `None` when the
    // point is behind the camera (`w <= 0`), where a divide would invent a
    // position rather than report one.
    let at = |u: f64, v: f64| -> Option<(f64, f64, bool)> {
        let sphere = per_tile.sphere((u * extent, v * extent));
        let lyon = glam::Vec3::new((u * lyon_per_unit) as f32, (v * lyon_per_unit) as f32, 0.0);
        let mut flat = (matrix * lyon.extend(1.0)).as_dvec4();
        // `vs_main` negates the MERCATOR x and nothing else -- see the
        // `clip.x = -clip.x` comment there. Omitting it here would make the
        // blend disagree with the shader by twice the flat term.
        flat.x = -flat.x;
        let clip = per_tile.project(flat, sphere);
        if clip.w <= 0.0 {
            return None;
        }
        let ndc = clip.truncate().truncate() / clip.w;
        Some((
            (ndc.x * 0.5 + 0.5) * w,
            (1.0 - (ndc.y * 0.5 + 0.5)) * h,
            per_tile.occluded(sphere),
        ))
    };

    // 33 samples a side: fine enough that a collapsed band cannot hide
    // between two samples, coarse enough to print.
    const N: usize = 32;
    let mut grid = Vec::with_capacity((N + 1) * (N + 1));
    for iv in 0..=N {
        for iu in 0..=N {
            let (u, v) = (iu as f64 / N as f64, iv as f64 / N as f64);
            grid.push(at(u, v));
        }
    }
    let idx = |iu: usize, iv: usize| iv * (N + 1) + iu;

    println!("# --explain-tile {}/{}/{}", tile.z, tile.x, tile.y);
    println!(
        "camera lon {} lat {} zoom {} size {}x{} transition {:.5}",
        request.center_lon,
        request.center_lat,
        request.zoom,
        request.width,
        request.height,
        frame.transition
    );
    // The four corners and the four edge midpoints, named, because a
    // collapsed edge is visible in eight numbers before any histogram.
    for (name, u, v) in [
        ("NW corner", 0.0, 0.0),
        ("NE corner", 1.0, 0.0),
        ("SW corner", 0.0, 1.0),
        ("SE corner", 1.0, 1.0),
        ("W edge mid", 0.0, 0.5),
        ("E edge mid", 1.0, 0.5),
        ("N edge mid", 0.5, 0.0),
        ("S edge mid", 0.5, 1.0),
    ] {
        match at(u, v) {
            Some((x, y, occ)) => println!(
                "  {name:<11} u={u:.1} v={v:.1} -> ({x:8.2}, {y:8.2}){}",
                if occ { "  OCCLUDED" } else { "" }
            ),
            None => println!("  {name:<11} u={u:.1} v={v:.1} -> behind the camera"),
        }
    }
    // Edge lengths in pixels. The west and east edges of a tile are mirror
    // images about the camera's own meridian when the camera sits on the
    // tile's centre longitude, so a large asymmetry between them is a
    // defect and not a projection.
    let edge_len = |pts: &[(usize, usize)]| -> f64 {
        let mut total = 0.0;
        for pair in pts.windows(2) {
            if let (Some(a), Some(b)) =
                (grid[idx(pair[0].0, pair[0].1)], grid[idx(pair[1].0, pair[1].1)])
            {
                total += ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt();
            }
        }
        total
    };
    let west: Vec<(usize, usize)> = (0..=N).map(|iv| (0, iv)).collect();
    let east: Vec<(usize, usize)> = (0..=N).map(|iv| (N, iv)).collect();
    let north: Vec<(usize, usize)> = (0..=N).map(|iu| (iu, 0)).collect();
    let south: Vec<(usize, usize)> = (0..=N).map(|iu| (iu, N)).collect();
    println!(
        "edges px: W {:.1}  E {:.1}  N {:.1}  S {:.1}",
        edge_len(&west),
        edge_len(&east),
        edge_len(&north),
        edge_len(&south)
    );

    // Screen area per grid cell, by quarter of the tile in u (west to
    // east). A uniform projection gives the four quarters comparable
    // totals; a collapse empties one of them.
    let mut quarter_area = [0.0f64; 4];
    let mut quarter_cells = [0usize; 4];
    let mut collapsed = [0usize; 4];
    let mut all_areas = Vec::new();
    let mut cell_area = vec![None; N * N];
    // THREE THINGS THE ORIGINAL SUMMARY CANNOT SEE, added for the sliver.
    //
    // A sliver is a cell that EXPANDS, and every existing signal here is
    // tuned for one that shrinks:
    //
    //  - `area.abs()` below made an INVERTED cell (one whose winding has
    //    flipped, i.e. the projection folded the quad over on itself)
    //    arithmetically identical to a healthy one;
    //  - "collapsed" is `< 1% of median`, so a cell that blows up to 100x
    //    the median -- exactly a small coastal quad becoming a wedge across
    //    open water -- scored as perfectly normal;
    //  - a cell with any corner at `w <= 0` was `continue`d, counted in no
    //    bucket at all. That is the most suspicious cell on the grid: the
    //    GPU does not skip it, it interpolates and clips, and a vertex just
    //    on the near side of `w == 0` projects arbitrarily far off screen.
    let mut signed_cell = vec![None; N * N];
    let mut skipped = [0usize; 4];
    for iv in 0..N {
        for iu in 0..N {
            let q = (iu * 4) / N;
            let corners = [
                grid[idx(iu, iv)],
                grid[idx(iu + 1, iv)],
                grid[idx(iu + 1, iv + 1)],
                grid[idx(iu, iv + 1)],
            ];
            let Some(p) = corners.iter().copied().collect::<Option<Vec<_>>>() else {
                skipped[q] += 1;
                continue;
            };
            // Shoelace over the projected quad.
            let mut area = 0.0;
            for i in 0..4 {
                let (x0, y0, _) = p[i];
                let (x1, y1, _) = p[(i + 1) % 4];
                area += x0 * y1 - x1 * y0;
            }
            let signed = area * 0.5;
            let area = signed.abs();
            quarter_area[q] += area;
            quarter_cells[q] += 1;
            all_areas.push(area);
            cell_area[iv * N + iu] = Some(area);
            signed_cell[iv * N + iu] = Some(signed);
        }
    }
    let mut sorted = all_areas.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    // "Collapsed" against this tile's OWN median cell, so the threshold
    // travels with the camera instead of being a pixel count that means
    // something different at every zoom.
    for iv in 0..N {
        for iu in 0..N {
            if let Some(a) = cell_area[iv * N + iu] {
                if a < median * 0.01 {
                    collapsed[(iu * 4) / N] += 1;
                }
            }
        }
    }
    // The dominant winding of this tile's cells; anything against it has
    // been folded over by the projection.
    let positive = signed_cell.iter().flatten().filter(|s| **s > 0.0).count();
    let negative = signed_cell.iter().flatten().filter(|s| **s < 0.0).count();
    let dominant_positive = positive >= negative;
    let mut inverted = [0usize; 4];
    let mut exploded = [0usize; 4];
    let mut worst_ratio = 0.0f64;
    let mut worst_at = (0usize, 0usize);
    for iv in 0..N {
        for iu in 0..N {
            let q = (iu * 4) / N;
            if let Some(s) = signed_cell[iv * N + iu] {
                if (s > 0.0) != dominant_positive && s != 0.0 {
                    inverted[q] += 1;
                }
            }
            if let Some(a) = cell_area[iv * N + iu] {
                if median > 0.0 {
                    let ratio = a / median;
                    if ratio > 100.0 {
                        exploded[q] += 1;
                    }
                    if ratio > worst_ratio {
                        worst_ratio = ratio;
                        worst_at = (iu, iv);
                    }
                }
            }
        }
    }
    println!("cell area px^2, by quarter of the tile west -> east (median cell {median:.2}):");
    for q in 0..4 {
        let mean =
            if quarter_cells[q] == 0 { 0.0 } else { quarter_area[q] / quarter_cells[q] as f64 };
        println!(
            "  quarter {q}: total {:10.1}  mean {:8.2}  cells {:4}  collapsed(<1% of median) {}",
            quarter_area[q], mean, quarter_cells[q], collapsed[q]
        );
    }
    println!(
        "EXPANSION/FOLD (the sliver's shape, which `collapsed` cannot see):\n  \
         inverted winding by quarter {inverted:?}  (total {})\n  \
         exploded >100x median by quarter {exploded:?}  (total {})\n  \
         skipped, a corner behind the camera (w <= 0) by quarter {skipped:?}  (total {})\n  \
         largest cell {:.1}x the median, at u-index {} of {N}, v-index {} of {N}",
        inverted.iter().sum::<usize>(),
        exploded.iter().sum::<usize>(),
        skipped.iter().sum::<usize>(),
        worst_ratio,
        worst_at.0,
        worst_at.1,
    );
    let total: f64 = quarter_area.iter().sum();
    println!("tile total {total:.1} px^2");
    Ok(())
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
    let args: Vec<String> = std::env::args().collect();
    let ab = args.iter().any(|a| a == "--ab");

    let mut request = CaptureRequest::default();
    if let Some(v) = flag(&args, "--projection") {
        // A flat world projection (`gpui_maps::projection`, spike): the
        // globe's per-vertex path drawing onto a plane.
        let projection = gpui_maps::projection::FlatProjection::parse(&v)
            .ok_or_else(|| anyhow::anyhow!("--projection {v}: expected equal-earth or robinson"))?;
        gpui_maps::projection::set_active(Some(projection));
    }
    if let Some(v) = flag(&args, "--lon") {
        request.center_lon = v.parse()?;
    }
    if let Some(v) = flag(&args, "--lat") {
        request.center_lat = v.parse()?;
    }
    if let Some(v) = flag(&args, "--zoom") {
        request.zoom = v.parse()?;
    }
    if let Some(v) = flag(&args, "--pitch") {
        request.pitch = v.parse::<f32>()?.to_radians();
    }
    if let Some(v) = flag(&args, "--bearing") {
        request.bearing = v.parse::<f32>()?.to_radians();
    }
    if args.iter().any(|a| a == "--flat") {
        request.flat = true;
        // OpenFreeMap's planet stops at 14; Mapbox Streets v8 at 16.
        request.flat_tile_zoom_max = if flag(&args, "--source").as_deref() == Some("mapbox") { 16 } else { 14 };
    }
    if let Some(v) = flag(&args, "--size") {
        // `N` for a square frame, `WxH` for any other.
        let (w, h) = match v.split_once('x') {
            Some((w, h)) => (w.parse::<u32>()?, h.parse::<u32>()?),
            None => (v.parse::<u32>()?, v.parse::<u32>()?),
        };
        request.width = w;
        request.height = h;
    }
    if let Some(v) = flag(&args, "--pixel-ratio") {
        request.pixel_ratio = v.parse::<f32>()?;
        anyhow::ensure!(request.pixel_ratio > 0.0, "--pixel-ratio takes a positive number, got {v}");
    }
    if args.iter().any(|a| a == "--plain") {
        // Plain, as `MapView::set_globe_enabled(false)` draws it: flat
        // Mercator at every zoom, the camera held inside the world
        // (`camera_control::plain_constrain`) unless `--unconstrained`.
        request.globe = false;
        request.flat = true;
        request.flat_tile_zoom_max = if flag(&args, "--source").as_deref() == Some("mapbox") { 16 } else { 14 };
        if !args.iter().any(|a| a == "--unconstrained") {
            let (w, h) = (f64::from(request.width) / f64::from(request.pixel_ratio), f64::from(request.height) / f64::from(request.pixel_ratio));
            let (lon, lat, zoom) = gpui_maps::camera_control::plain_constrain(request.center_lon, request.center_lat, request.zoom, w, h);
            if (lon, lat, zoom) != (request.center_lon, request.center_lat, request.zoom) {
                eprintln!("plain: constrained ({}, {}, z{}) to ({lon}, {lat}, z{zoom})", request.center_lon, request.center_lat, request.zoom);
            }
            (request.center_lon, request.center_lat, request.zoom) = (lon, lat, zoom);
        }
    }
    request.subdivision = match flag(&args, "--subdivision").as_deref() {
        None | Some("for-tile") => SubdivisionPolicy::ForTile,
        Some("none") => SubdivisionPolicy::None,
        Some(other) => anyhow::bail!("--subdivision takes `for-tile` or `none`, got `{other}`"),
    };
    request.clear = match flag(&args, "--clear").as_deref() {
        None | Some("style") => Clear::Style,
        Some("sentinel") => Clear::SENTINEL,
        // For `--paint wash`, where the pixel value must BE the composite
        // count: over black, one 0.3 coat is 77 and two are 130.
        Some("black") => Clear::Sentinel(gpui_maps::style::Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
        Some(other) => anyhow::bail!("--clear takes `style`, `sentinel` or `black`, got `{other}`"),
    };
    request.paint = match flag(&args, "--paint").as_deref() {
        None | Some("as-styled") => Paint::AsStyled,
        Some("flat") => Paint::FlattenedFills,
        Some("wash") => Paint::Wash,
        Some(other) => anyhow::bail!("--paint takes `as-styled`, `flat` or `wash`, got `{other}`"),
    };
    if args.iter().any(|a| a == "--lit") {
        // Clock-free and reproducible: the terminator crosses the middle of
        // the disc. See `CaptureRequest::sun`.
        request.sun = Some(gpui_maps::render::atmosphere::Sun::Viewport {
            tilt_deg: 90.0,
            bearing_deg: 0.0,
        });
    }
    if let Some(spec) = flag(&args, "--sun") {
        // A named subsolar point: the ground and the sky lit from it.
        let (lon, lat) = spec
            .split_once(',')
            .and_then(|(a, b)| Some((a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?)))
            .ok_or_else(|| anyhow::anyhow!("--sun takes LON,LAT in degrees, got `{spec}`"))?;
        request.sun = Some(gpui_maps::render::atmosphere::Sun::Map { lon_deg: lon, lat_deg: lat });
    }
    // `--space-look`: the opt-in planet-in-space look, from the textures
    // cached in `~/.cache/gpui-map/space/` (run
    // `scripts/fetch-space-assets.sh` first; a missing one is simply not
    // drawn). `--look-time UNIX` turns its sky (and the stars) to that
    // instant's sidereal angle.
    if args.iter().any(|a| a == "--space-look") {
        let textures = gpui_maps::space_assets::load_cached_textures();
        eprintln!(
            "space look: star map {}, clouds {}",
            textures.starmap.as_ref().map_or("missing".into(), |i| format!("{}x{}", i.width, i.height)),
            textures.clouds.as_ref().map_or("missing".into(), |i| format!("{}x{}", i.width, i.height)),
        );
        let at = match flag(&args, "--look-time") {
            Some(t) => Some(t.trim().parse::<f64>().map_err(|_| anyhow::anyhow!("--look-time takes Unix seconds, got `{t}`"))?),
            None => None,
        };
        request.space_look = Some((gpui_maps::render::space_look::SpaceLook::ON, Arc::new(textures), at));
    }
    if args.iter().any(|a| a == "--stars") {
        request.stars = true;
    }
    let wash = request.paint == Paint::Wash;
    if args.iter().any(|a| a == "--no-backdrop") {
        request.backdrop = false;
    }
    // Buildings: `CaptureRequest::extrusion_opacity` / `extrusion_flat`.
    request.extrusion_opacity = flag(&args, "--extrusion-opacity").map(|v| v.parse::<f32>()).transpose()?;
    if args.iter().any(|a| a == "--extrusion-flat") {
        request.extrusion_flat = true;
    }
    // `--extrusion-look enhanced|maplibre` (`ExtrusionOverrides::look`) and
    // `--label-occlusion` (`ExtrusionOverrides::label_occlusion`).
    match flag(&args, "--extrusion-look").as_deref() {
        None | Some("maplibre") => {}
        Some("enhanced") => request.extrusion_look = gpui_maps::render::extrusion::ExtrusionLook::ENHANCED,
        Some(other) => anyhow::bail!("--extrusion-look takes `enhanced` or `maplibre`, got `{other}`"),
    }
    if args.iter().any(|a| a == "--label-occlusion") {
        request.label_occlusion = true;
    }
    if args.iter().any(|a| a == "--labels") {
        // Point labels over the tiles, through `symbol_pass`. Off by
        // default because a label over a fill only gets in the way of
        // counting it -- see `CaptureRequest::labels`.
        request.labels = true;
    }
    if let Some(spec) = flag(&args, "--label-fonts") {
        // The browser/iOS font set instead of the system font book. See
        // `CaptureRequest::label_fonts`.
        let (family, files) = spec
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--label-fonts takes FAMILY=FILE,FILE,..., got `{spec}`"))?;
        let bytes = files
            .split(',')
            .map(|file| std::fs::read(file.trim()).map_err(|err| anyhow::anyhow!("--label-fonts {file}: {err}")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        request.label_fonts = Some((bytes.into(), family.trim().to_string()));
    }
    if args.iter().any(|a| a == "--atmosphere") {
        // The frame's own sun (`--sun`, else the clock-free one `--lit`
        // uses), so a lit frame and its glow agree about where the
        // terminator is.
        request.atmosphere = Some(gpui_maps::render::atmosphere::Atmosphere {
            sun: request
                .sun
                .unwrap_or(gpui_maps::render::atmosphere::Sun::Viewport { tilt_deg: 90.0, bearing_deg: 0.0 }),
            // The terminator ON, named: `--sun`/`--lit` ask to see it, and
            // the engine default is now off (see `--engine-default`).
            night_brightness: gpui_maps::render::atmosphere::Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS,
            ..Default::default()
        });
    }
    // `--engine-default UNIX`: what a freshly built `MapView` draws at that
    // wall-clock instant -- its real sun (`Sun::at_utc`), its default
    // `Space` (terminator, stars, glow as the engine ships them). The
    // plain-default proof: a city at local midnight must come out daylit.
    if let Some(spec) = flag(&args, "--engine-default") {
        let unix: f64 = spec.trim().parse().map_err(|_| anyhow::anyhow!("--engine-default takes a Unix time in seconds, got `{spec}`"))?;
        let space = gpui_maps::render::space::Space::default();
        // `frame_sun_for` with the instant as its clock: exactly the sun
        // `MapView::paint` resolves for a default `Space` at that instant
        // (daylight while the terminator is off, the real sun once it is on).
        let sun = gpui_maps::render::atmosphere::frame_sun_for(&space.atmosphere, Some(unix));
        request.sun = Some(sun);
        request.atmosphere = Some(gpui_maps::render::atmosphere::Atmosphere { sun, ..space.atmosphere });
        request.stars = space.star_intensity > 0.0;
    }
    // `--nav-route <path> [--nav-at <metres>]`: a paused simulated drive at
    // that distance along the route, its follow camera settled, and the
    // route line and chrome drawn over the map as `MapView::paint` draws
    // them. The follow camera REPLACES `--lon`/`--lat`/`--zoom`/`--pitch`/
    // `--bearing`; pass `--flat` for the tilted street frame.
    if let Some(route_path) = flag(&args, "--nav-route") {
        use gpui_maps::nav::host::{NavFrame, NavHost, SafeInsets, SimOptions, WallClock};
        let at_m: f64 = flag(&args, "--nav-at").map(|v| v.parse()).transpose()?.unwrap_or(0.0);
        let bytes = std::fs::read(&route_path).map_err(|e| anyhow::anyhow!("--nav-route {route_path}: {e}"))?;
        let mut host = NavHost::new();
        host.set_demo_route(
            std::sync::Arc::from(bytes),
            SimOptions { start_at_m: Some(at_m), paused: true, ..Default::default() },
        );
        let mut camera = gpui_maps::camera_control::CameraController::new(
            request.camera(),
            gpui_maps::camera_control::MIN_ZOOM,
            gpui_maps::camera_control::MAX_ZOOM,
        );
        // A fixed wall clock (UTC+8, Manila and Singapore) so a capture's
        // ETA is reproducible.
        let frame_at = |now_ms: f64| NavFrame {
            now_ms,
            wall: WallClock { unix_ms: 1_773_755_400_000.0 + now_ms, utc_offset_min: 480 },
            viewport: (request.width as f32, request.height as f32),
            insets: SafeInsets::default(),
        };
        if let Some(name) = flag(&args, "--nav-camera") {
            let mode = gpui_maps::nav::host::NavCameraMode::from_name(&name)
                .ok_or_else(|| anyhow::anyhow!("--nav-camera takes follow3d, follow2d, northup2d or overview, got `{name}`"))?;
            host.set_nav_camera_mode(mode, &camera, &frame_at(0.0));
        }
        if let Some(name) = flag(&args, "--nav-proximity") {
            let offset = gpui_maps::nav::follow::zoom_offset_preset(&name)
                .ok_or_else(|| anyhow::anyhow!("--nav-proximity takes close, standard or far, got `{name}`"))?;
            host.set_nav_follow_zoom_offset(offset, &frame_at(0.0));
        }
        if let Some(value) = flag(&args, "--nav-zoom-offset") {
            let offset: f64 = value.parse().map_err(|_| anyhow::anyhow!("--nav-zoom-offset takes a number, got `{value}`"))?;
            host.set_nav_follow_zoom_offset(offset, &frame_at(0.0));
        }
        host.start_simulation(&camera, &frame_at(0.0)).map_err(|e| anyhow::anyhow!("nav: {e}"))?;
        let _ = host.advance(&mut camera, &frame_at(0.0));
        // The mode-change glide lasts 800 ms; at 1000 the camera is on the
        // follow pose.
        let _ = host.advance(&mut camera, &frame_at(1000.0));
        // `--nav-zoom-by L`: a wheel zoom mid-drive, as the user's; the
        // camera keeps following and takes it as its offset.
        let mut captured_at = 1000.0;
        if let Some(value) = flag(&args, "--nav-zoom-by") {
            let levels: f64 = value.parse().map_err(|_| anyhow::anyhow!("--nav-zoom-by takes a number of zoom levels, got `{value}`"))?;
            let (w, h) = (request.width as f32, request.height as f32);
            camera.apply(gpui_maps::camera_control::CameraIntent::ZoomAbout { levels, anchor_px: (w / 4.0, h / 4.0), immediate: true });
            let mut t = 1200.0;
            while t < 2400.0 {
                t += 16.0;
                let _ = host.advance(&mut camera, &frame_at(t));
            }
            captured_at = t;
        }
        let settled = if args.iter().any(|a| a == "--nav-own-camera") {
            request.camera()
        } else {
            *camera.camera()
        };
        request.center_lon = settled.center_lon;
        request.center_lat = settled.center_lat;
        request.zoom = settled.zoom;
        request.pitch = settled.pitch;
        request.bearing = settled.bearing;
        eprintln!(
            "globe_capture: nav camera lon {:.5} lat {:.5} zoom {:.2} pitch {:.1} bearing {:.1}",
            settled.center_lon,
            settled.center_lat,
            settled.zoom,
            settled.pitch.to_degrees(),
            settled.bearing.to_degrees()
        );
        if let Some(state) = host.state() {
            eprintln!("globe_capture: nav state {state:?}");
        }
        request.nav = Some(gpui_maps::capture::NavCapture { host: std::sync::Arc::new(host), frame: frame_at(captured_at) });
    }
    if let Some(path) = flag(&args, "--overlay") {
        request.overlay =
            Some(std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("--overlay {path}: {e}"))?);
    }
    if let Some(path) = flag(&args, "--overlay-paint") {
        request.overlay_paint = Some(std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("--overlay-paint {path}: {e}"))?);
    }
    if let Some(path) = flag(&args, "--arcs") {
        request.arcs.push(arc_layer(&path)?);
    }
    // `--explain-tile` answers a question about GEOMETRY, so it runs before
    // any style is compiled, any byte is fetched and any GPU is opened, and
    // returns without drawing. It needs the camera and nothing else.
    if args.iter().any(|a| a == "--synthetic-shells") {
        request.altitude = Some(synthetic_shells());
    }
    #[cfg(feature = "orbits")]
    if let Some(path) = flag(&args, "--satellites") {
        let select = flag(&args, "--select")
            .map(|v| v.parse::<u32>().map_err(|e| anyhow::anyhow!("--select takes a NORAD id, got `{v}`: {e}")))
            .transpose()?;
        let halo = flag(&args, "--halo")
            .map(|v| v.parse::<u32>().map_err(|e| anyhow::anyhow!("--halo takes a NORAD id, got `{v}`: {e}")))
            .transpose()?;
        let options = SatelliteOptions {
            at: flag(&args, "--at"),
            select,
            halo,
            groups: flag(&args, "--groups").map(PathBuf::from),
            dim_others: args.iter().any(|a| a == "--dim-others"),
        };
        let (altitude, track) = satellites(&path, &options)?;
        request.altitude = Some(altitude);
        if let Some((geojson, paint)) = track {
            request.overlay = Some(geojson);
            request.overlay_paint = Some(paint);
        }
    }
    #[cfg(feature = "orbits")]
    for dependent in ["--select", "--halo", "--groups", "--dim-others"] {
        if args.iter().any(|a| a == dependent) && flag(&args, "--satellites").is_none() {
            anyhow::bail!("{dependent} needs --satellites FILE");
        }
    }
    #[cfg(not(feature = "orbits"))]
    for gated in ["--satellites", "--select", "--halo", "--groups", "--dim-others"] {
        if args.iter().any(|a| a == gated) {
            anyhow::bail!("{gated} needs `--features orbits`");
        }
    }
    if let Some(spec) = flag(&args, "--explain-tile") {
        return explain_tile(&request, parse_tile(&spec, "--explain-tile")?);
    }
    if args.iter().any(|a| a == "--explain-cover") {
        return explain_cover(&request);
    }

    let out = flag(&args, "--out").map(PathBuf::from);
    // `openfreemap` or `mapbox`. Named here rather than at the fetch,
    // because the CACHE DIRECTORY depends on it: both sources name their
    // bytes `z-x-y.pbf`, so one shared directory would silently serve
    // OpenFreeMap tiles to a Mapbox run and every count taken from the
    // frame would be a count of the wrong map.
    let source_name = flag(&args, "--source").unwrap_or_else(|| "openfreemap".to_string());
    let cache = flag(&args, "--cache").map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("gpui-map-capture-tiles-{source_name}"))
    });
    // `--raster <name>` draws an imagery basemap under the vector layers.
    // Absent by default, so every existing invocation of this harness
    // renders the frame it rendered before.
    let raster_name = flag(&args, "--raster");
    // `--raster-opacity` overrides what the style's raster layer resolves
    // to, for the A/B that grades the property itself. `None` means "ask
    // the style", which is what a production frame does.
    let raster_opacity_override =
        flag(&args, "--raster-opacity").map(|v| v.parse::<f32>()).transpose()?;

    let style_path =
        flag(&args, "--style").unwrap_or_else(|| "examples/liberty-style.json".to_string());
    let mut style_bytes = std::fs::read(&style_path)
        .map_err(|e| anyhow::anyhow!("run from the repo root ({style_path}): {e}"))?;
    // `--buildings-3d`: opt the style into 3D buildings before compiling,
    // exactly as an app would -- the vendored file itself is never edited.
    if args.iter().any(|a| a == "--buildings-3d") {
        let mut doc: serde_json::Value = serde_json::from_slice(&style_bytes)?;
        gpui_maps::style_spec::with_3d_buildings(&mut doc)?;
        style_bytes = serde_json::to_vec(&doc)?;
    }
    let mut style = gpui_maps::style_spec::parse_and_compile(&style_bytes)?;
    // `--layer-off a,b`: drop whole style layers before anything is baked,
    // so "is it THIS layer" is a frame rather than an argument. One JSON
    // layer can compile into several `StyleLayer`s sharing its id, so this
    // matches on the id and drops all of them.
    if let Some(list) = flag(&args, "--layer-off") {
        let off: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        let before = style.layers.len();
        style.layers.retain(|layer| !off.contains(&layer.id()));
        eprintln!(
            "style: {style_path}, {} layers, {} dropped by --layer-off {list}",
            style.layers.len(),
            before - style.layers.len()
        );
    } else if let Some(list) = flag(&args, "--only-layer") {
        // The complement, and the one that makes a composite count mean
        // something on a 134-layer style: with every other fill dropped,
        // a pixel's coat count is this layer's OWN overdraw and nothing
        // else's. `--layer-off` on a 203-layer compiled style cannot do
        // that without naming 202 ids.
        let keep: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        style.layers.retain(|layer| keep.contains(&layer.id()));
        eprintln!("style: {style_path}, {} layers kept by --only-layer {list}", style.layers.len());
    } else {
        eprintln!("style: {style_path}, {} layers", style.layers.len());
    }

    // The building floor, resolved as MapView and `capture_async` resolve
    // it, BEFORE the cover is fetched; `--no-building-floor` for the control.
    request.disable_building_floor = args.iter().any(|a| a == "--no-building-floor");
    request = request.resolved(&style);

    // `--absent N`: treat the LAST N tiles of the cover as still loading,
    // so the capture draws their ancestor underneath the ready ones -- the
    // production stand-in path. The last N rather than the first because
    // `GlobeCover::cover` sorts by visibility score, so the tail is the
    // edge of the disc, which is where tiles really are still arriving.
    let absent_count: usize = flag(&args, "--absent").map(|v| v.parse()).transpose()?.unwrap_or(0);
    // `--absent-head N` takes the FIRST N instead: the most visible tiles
    // of the cover, squarely in the middle of the disc. The tail is where
    // tiles really are still arriving, but a tail tile can be so close to
    // the limb that it covers no pixels at all -- and then a stand-in that
    // drew nothing would score exactly as well as one that drew the tile,
    // so the coverage half of the grading would never be exercised. Head
    // tiles are where a hole would be impossible to miss.
    let absent_head: usize =
        flag(&args, "--absent-head").map(|v| v.parse()).transpose()?.unwrap_or(0);

    let mut cover = request.cover();
    // `--only-tile z/x/y`: draw ONE tile of the cover and nothing else.
    // The whole point is to ask whether a defect lives INSIDE one tile's
    // own projected geometry or between neighbours -- a question a full
    // cover cannot separate.
    if let Some(spec) = flag(&args, "--only-tile") {
        let mut want = Vec::new();
        for one in spec.split(',') {
            let parts: Vec<&str> = one.split('/').collect();
            anyhow::ensure!(parts.len() == 3, "--only-tile takes z/x/y[,z/x/y...], got `{spec}`");
            want.push(gpui_maps::geo::TileId {
                z: parts[0].parse()?,
                x: parts[1].parse()?,
                y: parts[2].parse()?,
            });
        }
        cover.retain(|t| want.contains(t));
        anyhow::ensure!(!cover.is_empty(), "none of {want:?} is in this camera's cover");
    }
    let cover = cover;
    anyhow::ensure!(
        !cover.is_empty(),
        "this camera has no globe cover — zoom {} is outside the globe band \
         (`render::globe::GLOBE_ZOOM_MIN`..`GLOBE_ZOOM_MAX`), and this harness only \
         draws the globe",
        request.zoom
    );
    eprintln!(
        "camera lon {} lat {} zoom {} -> display zoom {}, cover {} tiles",
        request.center_lon,
        request.center_lat,
        request.zoom,
        request.display_zoom(),
        cover.len()
    );

    // The ancestors a stand-in run needs, fetched alongside the cover --
    // unless `--no-stand-in` withholds them, which is the CONTROL for the
    // stand-in's whole reason to exist: with no ancestor in the store the
    // missing tile is simply not drawn, and the frame has a bare hole where
    // it should be. Read the `coverage:` line of a `--no-stand-in` run
    // against the same run without it, and the difference is what the
    // stand-in is buying. A fix to the stand-in that cured a double
    // composite by drawing nothing would read the same as this control,
    // and that is how to tell.
    let no_stand_in = args.iter().any(|a| a == "--no-stand-in");
    request.absent = cover
        .iter()
        .rev()
        .take(absent_count)
        .chain(cover.iter().take(absent_head))
        .copied()
        .collect();
    let mut to_fetch = cover.clone();
    if !no_stand_in {
        for tile in &request.absent {
            if tile.z > 0 {
                let parent = gpui_maps::geo::TileId { z: tile.z - 1, x: tile.x / 2, y: tile.y / 2 };
                if !to_fetch.contains(&parent) {
                    to_fetch.push(parent);
                }
            }
        }
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let source: Arc<dyn TileSource> = match source_name.as_str() {
        "openfreemap" => Arc::new(HttpTileSource::new(OPENFREEMAP, 14, ATTRIBUTION)),
        "mapbox" => mapbox_source()?,
        other => anyhow::bail!("--source takes `openfreemap` or `mapbox`, got `{other}`"),
    };
    let tiles = runtime.block_on(fetch_cover(source.as_ref(), &to_fetch, &cache))?;
    if absent_count > 0 {
        eprintln!(
            "stand-in: {} of {} cover tiles treated as still loading, their ancestors drawn under \
             the ready ones",
            request.absent.len(),
            cover.len()
        );
    }
    eprintln!("tiles: {} of {} decoded (cache: {})", tiles.len(), to_fetch.len(), cache.display());

    if let Some(name) = raster_name.as_deref() {
        let (raster_src, tile_size, suffix) = raster_source(name)?;
        // Its OWN cache directory. A raster tile and a vector tile share a
        // `z/x/y` and would share a filename; keeping them apart is what
        // stops PNG bytes reaching `mvt::decode`, and it is the same
        // licensing separation the vector cache's source suffix makes.
        let raster_cache =
            std::env::temp_dir().join(format!("gpui-map-capture-raster-{suffix}"));
        // The cover only -- no stand-in ancestors. A raster tile that has
        // not arrived is simply not drawn; standing an ancestor in for it
        // needs the reference's sub-rectangle sampling
        // (`raster.vertex.glsl:59`) and is deliberately not built.
        let raster_tiles =
            fetch_raster_cover(&runtime, raster_src.as_ref(), &cover, tile_size, &raster_cache)?;
        eprintln!(
            "raster: {} of {} tiles decoded from {name} (cache: {})",
            raster_tiles.len(),
            cover.len(),
            raster_cache.display()
        );
        if let Some(credit) = raster_src.attribution() {
            // A licence obligation, printed rather than drawn: this
            // harness renders no chrome. CC BY 4.0 on the satellite source
            // means a frame published from it owes this line.
            eprintln!("raster attribution: {credit}");
        }
        // The style's own `raster-opacity` at this zoom, unless overridden.
        // Liberty's `natural_earth` resolves to 0.6 at z0 and 0.1 at z6,
        // so a frame rendered without this would be a frame of the wrong
        // map.
        let from_style = style.layers.iter().find_map(|layer| match layer {
            gpui_maps::style::StyleLayer::Raster { opacity, .. } => {
                Some(opacity.value_at(request.display_zoom()))
            }
            _ => None,
        });
        request.raster_opacity = raster_opacity_override.or(from_style).unwrap_or(1.0);
        eprintln!("raster opacity: {}", request.raster_opacity);
        request.raster = raster_tiles;
    }

    if ab {
        let mut for_tile = request.clone();
        for_tile.subdivision = SubdivisionPolicy::ForTile;
        let mut none = request.clone();
        none.subdivision = SubdivisionPolicy::None;

        let a = capture(&for_tile, &tiles, &style)?;
        let b = capture(&none, &tiles, &style)?;
        report("Subdivision::for_tile", &a, wash);
        report("Subdivision::NONE", &b, wash);

        let (differing, worst) = a.diff(&b, 0);
        let total = (a.width * a.height) as usize;
        println!(
            "\nA/B  for_tile vs NONE: {differing} of {total} pixels differ \
             ({:.3}%), worst channel difference {worst}",
            differing as f64 / total as f64 * 100.0
        );
        if let Some(out) = &out {
            let stem = out.with_extension("");
            a.save_png(&PathBuf::from(format!("{}-for-tile.png", stem.display())))?;
            b.save_png(&PathBuf::from(format!("{}-none.png", stem.display())))?;
            println!("wrote {}-for-tile.png and {}-none.png", stem.display(), stem.display());
        }
        return Ok(());
    }

    if let Some(n) = flag(&args, "--time") {
        return time_look_ab(&request, &tiles, &style, n.parse()?);
    }

    let frame = capture(&request, &tiles, &style)?;
    report(&format!("{:?}", request.subdivision), &frame, wash);
    if let Some(out) = &out {
        frame.save_png(out)?;
        println!("wrote {}", out.display());
    }
    Ok(())
}

/// `--time N`: the space look's full-frame A/B. One session renders the
/// request `N` times per arm, the arms INTERLEAVED (look off, look on, ...)
/// so neither pays the other's warm-up, after two untimed frames each.
/// Prints each arm's wall ms per frame (encode + GPU + readback) and the
/// session's stage split, p50/p95/max. Needs `--space-look`.
fn time_look_ab(
    request: &CaptureRequest,
    tiles: &gpui_maps::capture::TileStore,
    style: &gpui_maps::style::Style,
    n: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(request.space_look.is_some(), "--time needs --space-look (the ON arm)");
    anyhow::ensure!(n > 0, "--time takes a positive count");
    let mut off = request.clone();
    off.space_look = None;
    let arms = [("look off", off), ("look on", request.clone())];
    let mut session = gpui_maps::capture::CaptureSession::new_blocking(None)?;
    for (_, r) in &arms {
        for _ in 0..2 {
            session.render(r, tiles, style)?;
        }
    }
    let mut wall = [Vec::new(), Vec::new()];
    let before = gpui_maps::render::allocation_stats().draw_calls_total;
    for _ in 0..n {
        for (i, (_, r)) in arms.iter().enumerate() {
            let t = std::time::Instant::now();
            session.render(r, tiles, style)?;
            wall[i].push(t.elapsed().as_secs_f64() * 1e3);
        }
    }
    // Gate on proof of paint.
    let draws = gpui_maps::render::allocation_stats().draw_calls_total - before;
    anyhow::ensure!(draws > 0, "no draw call issued: nothing was timed");
    println!("time: {}x{} z{} draws {draws}", request.width, request.height, request.zoom);
    for (i, (label, _)) in arms.iter().enumerate() {
        let w = &mut wall[i];
        w.sort_by(f64::total_cmp);
        let q = |p: f64| w[((w.len() - 1) as f64 * p).round() as usize];
        println!("time: {label:8} wall p50 {:.2} p95 {:.2} max {:.2} ms", q(0.5), q(0.95), q(1.0));
    }
    Ok(())
}

/// `0xRRGGBB` to an opaque `Rgba`.
fn hex(c: u32) -> gpui_maps::style::Rgba {
    let ch = |shift: u32| f32::from(((c >> shift) & 0xFF) as u8) / 255.0;
    gpui_maps::style::Rgba { r: ch(16), g: ch(8), b: ch(0), a: 1.0 }
}

/// The spec §1.2 group colours, by paint index (stations, Starlink, GPS,
/// GEO, other), restated so `--synthetic-shells` runs without `orbits`;
/// `orbit::GROUP_COLOURS` holds the same values.
const GROUP_HEX: [u32; 5] = [0xFF5A36, 0x3D8BFF, 0xF5A623, 0xA974F0, 0x8C99A6];

/// An altitude style painted by group: the §1.2 colours, §2.5's sizes
/// (Starlink 1.6 px, the rest 2), and the default 60%-black outline.
fn group_style(paint: Vec<u8>) -> gpui_maps::render::altitude_pipeline::AltitudeStyle {
    let mut style = gpui_maps::render::altitude_pipeline::AltitudeStyle { paint, ..Default::default() };
    for (k, &c) in GROUP_HEX.iter().enumerate() {
        style.palette[k] = hex(c);
    }
    style.base_px[1] = 1.6;
    style
}

/// Spec §5.3's synthetic scene: `tests/altitude_capture.rs`'s two rings (a
/// GEO ring every 5 degrees, a 550 km ring every 10) plus a static Walker
/// 53° shell of 2,000 at 550 km (40 planes of 50, phasing 1).
fn synthetic_shells() -> gpui_maps::capture::AltitudeCapture {
    use gpui_maps::render::altitude::GeoPoint;
    let mut points = Vec::new();
    let mut paint = Vec::new();
    for i in 0..72 {
        points.push(GeoPoint { lon: -180.0 + 5.0 * f64::from(i), lat: 0.0, alt_m: 35_786_000.0 });
        paint.push(3);
    }
    for i in 0..36 {
        points.push(GeoPoint { lon: -180.0 + 10.0 * f64::from(i), lat: 0.0, alt_m: 550_000.0 });
        paint.push(1);
    }
    let (planes, per_plane, phasing) = (40u32, 50u32, 1u32);
    let inc = 53f64.to_radians();
    for k in 0..planes {
        let raan = std::f64::consts::TAU * f64::from(k) / f64::from(planes);
        for j in 0..per_plane {
            let u = std::f64::consts::TAU * (f64::from(j) / f64::from(per_plane) + f64::from(phasing * k) / f64::from(planes * per_plane));
            let lat = (inc.sin() * u.sin()).asin();
            let lon = raan + (inc.cos() * u.sin()).atan2(u.cos());
            let lon = (lon.to_degrees() + 540.0).rem_euclid(360.0) - 180.0;
            points.push(GeoPoint { lon, lat: lat.to_degrees(), alt_m: 550_000.0 });
            paint.push(1);
        }
    }
    gpui_maps::capture::AltitudeCapture { points, style: group_style(paint), ..Default::default() }
}

/// `--satellites FILE --at UTC`: every object in the OMM file propagated to
/// `at` (the median epoch by default), TEME to Earth-fixed by GMST, as a
/// geocentric point above the sphere, painted by group.
///
/// `--select NORAD` adds that object's orbit ring (one period from `at`,
/// 256 samples, inertial, 1.5 px, its group colour at 70% alpha), gives it
/// the white halo, and returns its ground track (`[t - P/2, t + P]`) as an
/// overlay GeoJSON and line paint in the same colour, 1.5 px.
///
/// `--groups DIR` classifies by the cached group lists, `--halo NORAD`
/// moves the halo, and `--dim-others` fades every other group to 60%.
#[cfg(feature = "orbits")]
struct SatelliteOptions {
    at: Option<String>,
    select: Option<u32>,
    halo: Option<u32>,
    groups: Option<PathBuf>,
    dim_others: bool,
}

/// `--groups DIR`: the NORAD ids of `DIR/{stations,gps-ops,geo}.json`, each
/// `None` when its file is missing (that group then classifies nothing). A
/// file that is present but does not parse is an error, not a silent grey.
#[cfg(feature = "orbits")]
fn membership_from(dir: Option<&std::path::Path>) -> anyhow::Result<gpui_maps::orbit::catalog::Membership> {
    use gpui_maps::orbit::{catalog::Membership, omm::parse_omm_reader};
    let Some(dir) = dir else { return Ok(Membership::default()) };
    let ids = |group: &str| -> anyhow::Result<Option<std::collections::HashSet<u32>>> {
        let path = dir.join(format!("{group}.json"));
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("satellites: --groups: no {}, `{group}` left unclassified", path.display());
                return Ok(None);
            }
            Err(e) => anyhow::bail!("--groups {}: {e}", path.display()),
        };
        let report = parse_omm_reader(std::io::BufReader::new(file))
            .map_err(|e| anyhow::anyhow!("--groups {}: {e:?}", path.display()))?;
        Ok(Some(report.records.iter().map(|r| r.norad_id).collect()))
    };
    Ok(Membership { stations: ids("stations")?, gps: ids("gps-ops")?, geo: ids("geo")?, ..Membership::default() })
}

#[cfg(feature = "orbits")]
#[allow(clippy::type_complexity)]
fn satellites(
    path: &str,
    options: &SatelliteOptions,
) -> anyhow::Result<(gpui_maps::capture::AltitudeCapture, Option<(String, String)>)> {
    use gpui_maps::orbit::{catalog::Catalog, omm::parse_omm_reader, propagate};
    use gpui_maps::render::altitude::GeoPoint;
    let select = options.select;
    let halo = options.halo.or(select);
    let file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("--satellites {path}: {e}"))?;
    let report = parse_omm_reader(std::io::BufReader::new(file)).map_err(|e| anyhow::anyhow!("--satellites {path}: {e:?}"))?;
    let catalog = Catalog::build(report.records, &membership_from(options.groups.as_deref())?);
    let at_unix = match options.at.as_deref() {
        Some(s) => {
            let t = chrono::NaiveDateTime::parse_from_str(s.trim().trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S%.f")
                .map_err(|e| anyhow::anyhow!("--at takes RFC 3339 UTC, e.g. 2026-09-28T11:09:16Z: {e}"))?
                .and_utc();
            t.timestamp() as f64 + f64::from(t.timestamp_subsec_nanos()) * 1e-9
        }
        None => catalog.epoch_stats_unix().ok_or_else(|| anyhow::anyhow!("--satellites {path}: no objects"))?.1,
    };
    let gmst = gpui_maps::orbit::gmst_rad(at_unix);
    let (mut points, mut paint, mut hidden) = (Vec::new(), Vec::new(), 0usize);
    let mut selected_point = None;
    for o in &catalog.objects {
        let Some(state) = propagate::state_at(o, at_unix) else {
            hidden += 1;
            continue;
        };
        if Some(o.record.norad_id) == halo {
            selected_point = Some(points.len());
        }
        let [x, y, z] = propagate::teme_to_ecef_km(state.position_km, gmst);
        let r = (x * x + y * y + z * z).sqrt();
        points.push(GeoPoint {
            lon: y.atan2(x).to_degrees(),
            lat: (z / r).asin().to_degrees(),
            alt_m: r * 1000.0 - gpui_maps::render::globe::GLOBE_RADIUS_M,
        });
        paint.push(o.group as u8);
    }
    eprintln!(
        "satellites: drawn {}, hidden by sgp4 {hidden} (skipped at init {}), group counts {:?} [stations, starlink, gps, geo, other, galileo, glonass, beidou]",
        points.len(),
        catalog.skipped_by_sgp4,
        catalog.group_counts()
    );
    let (sun_lon, sun_lat) = gpui_maps::render::solar::subsolar_point(at_unix);
    eprintln!("satellites: scene time {at_unix:.0}, subsolar point (for --sun) {sun_lon:.2},{sun_lat:.2}");
    if let Some(i) = selected_point {
        let p = &points[i];
        eprintln!("satellites: halo object at lon {:.2} lat {:.2} alt {:.0} km", p.lon, p.lat, p.alt_m / 1000.0);
    }
    let mut style = group_style(paint);
    if options.dim_others {
        let norad = halo.ok_or_else(|| anyhow::anyhow!("--dim-others needs --select or --halo"))?;
        let index = catalog.index_of(norad).ok_or_else(|| anyhow::anyhow!("--halo {norad}: not in {path}"))?;
        let keep = catalog.objects[index].group as usize;
        for (k, colour) in style.palette.iter_mut().enumerate().take(GROUP_HEX.len()) {
            if k != keep {
                colour.a = 0.6;
            }
        }
    }
    let mut altitude = gpui_maps::capture::AltitudeCapture { points, style, ..Default::default() };
    altitude.emphasis = (None, None, selected_point);
    if options.halo.is_some() && selected_point.is_none() {
        anyhow::bail!("--halo {}: not drawn at the scene time", options.halo.unwrap_or_default());
    }
    let Some(norad) = select else { return Ok((altitude, None)) };
    let index = catalog.index_of(norad).ok_or_else(|| anyhow::anyhow!("--select {norad}: not in {path}"))?;
    let obj = &catalog.objects[index];
    let rgb = GROUP_HEX[(obj.group as usize).min(GROUP_HEX.len() - 1)];
    let colour_hex = format!("#{rgb:06x}");
    let ring = propagate::orbit_ring_teme(obj, at_unix, 256);
    let track = propagate::ground_track(obj, at_unix);
    eprintln!(
        "satellites: --select {norad} ({}): ring of {} samples, ground track of {} pieces / {} points",
        obj.record.object_name,
        ring.len(),
        track.len(),
        track.iter().map(Vec::len).sum::<usize>()
    );
    altitude.lines = vec![gpui_maps::render::altitude::AltitudeLine {
        points: ring,
        color: gpui_maps::style::Rgba { a: 0.7, ..hex(rgb) },
        closed: true,
    }];
    altitude.lines_frame = gpui_maps::render::altitude::RefFrame::Inertial;
    altitude.lines_width_px = 1.5;
    altitude.lines_scene_unix = at_unix;
    let geojson = propagate::ground_track_geojson(&track, &colour_hex);
    let paint = format!(r#"{{"line-color":"{colour_hex}"}}"#);
    Ok((altitude, Some((geojson, paint))))
}

/// What one frame is made of, and how much of it is the clear colour.
fn report(label: &str, frame: &Capture, wash: bool) {
    let total = (frame.width * frame.height) as usize;
    let sentinel = frame.count_matching([255, 0, 255, 255], 0);
    println!(
        "{label}: {} tiles drawn, {} missing, {} vertices, {} triangles, transition {:.3}",
        frame.tiles_drawn,
        frame.tiles_missing.len(),
        frame.vertices,
        frame.triangles,
        frame.transition,
    );
    if frame.labels_drawn > 0 {
        println!("  labels: {} placed", frame.labels_drawn);
    }
    if frame.label_occlusion.0 > 0 {
        println!("  label occlusion: {} POIs tested, {:.3} ms", frame.label_occlusion.0, frame.label_occlusion.1);
    }
    if frame.altitude_points_drawn > 0 {
        println!("  altitude: {} points handed to the point stage", frame.altitude_points_drawn);
    }
    if frame.raster_tiles_drawn > 0 {
        // The raster half of the "did anything get submitted" line above.
        // A raster tile tessellates nothing, so `vertices` says nothing
        // about it and a frame of pure imagery reports 0 there.
        println!("  raster: {} tiles drawn", frame.raster_tiles_drawn);
    }
    if !frame.tiles_missing.is_empty() {
        println!(
            "  WARNING: {} tiles of the cover had no bytes -- every hole count below is \
             contaminated by them: {:?}",
            frame.tiles_missing.len(),
            frame.tiles_missing
        );
    }
    println!(
        "  exact sentinel magenta: {sentinel} of {total} pixels ({:.4}%)",
        sentinel as f64 / total as f64 * 100.0
    );
    if wash {
        // Under `--paint wash --clear black` a pixel's value IS its
        // composite count. Printed here rather than left to a script,
        // because this number is the whole reason the mode exists: a pale
        // streak is a pixel that was composited twice.
        let (once, twice, thrice, more) = frame.wash_levels();
        println!(
            "  composites: {once} once, {twice} TWICE, {thrice} three times, {more} more \
             -- {:.4}% of covered pixels got more than one coat",
            f64::from(twice + thrice + more) / f64::from((once + twice + thrice + more).max(1))
                * 100.0
        );
        // The OTHER half of the bar. A fix that removes a double composite
        // by removing the stand-in altogether trades a pale patch for a
        // missing one, which is worse -- so the coverage is printed beside
        // the doubling, always, and read together with it.
        println!(
            "  coverage: {} covered pixels, {} enclosed background pixels",
            once + twice + thrice + more,
            frame.enclosed_background([0, 0, 0, 255], 0),
        );
    }
}

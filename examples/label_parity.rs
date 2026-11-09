//! Which labels this engine places, at one camera, with no window.
//!
//! # What this is for
//!
//! `web/` draws the same map as the native build, from the same `src/`, and
//! the claim that both place the SAME labels needs to be checkable without
//! two screenshots and a squint. Since `symbol_pass.rs` moved out of
//! `map_view.rs`, everything between a baked `SymbolFeature` and a
//! `glyphon::TextArea` is one code path -- so this example runs that path
//! directly, prints the labels it places, and the browser's
//! `map_labels_json()` prints its own. Two lists, diffed.
//!
//! No `gpui`, no `wgpu`, no window, no display. Shaping needs a
//! `FontSystem` and nothing else, and collision needs no GPU at all, so the
//! whole pass runs on a plain tokio main. That matters beyond convenience:
//! a window has to be foreground for macOS to composite it, and a run that
//! never got focus has historically been read as a rendering failure when
//! `Element::paint` had simply never run.
//!
//! # The font arm is the point
//!
//! Run it twice. `--fonts system` uses the machine's font book, which is
//! what the native app really does; `--fonts <path>[,<path>]` loads exactly
//! the faces the browser build fetched. The two arms place DIFFERENT label
//! sets, and that is not a bug: a label's collision box is measured from
//! the shaped glyphs, so a different face is a different box is a different
//! set of survivors. Holding the browser against the `system` arm measures
//! the fonts; holding it against the subset arm measures the CODE.
//!
//! ```text
//! cargo run --release --example label_parity -- \
//!     --style examples/mapbox-style.json --lon 139.7671 --lat 35.6812 --zoom 14 \
//!     --width 1280 --height 713 --dpr 1 \
//!     --fonts assets/fonts/NotoSansJP-Regular.japan-subset.otf,assets/fonts/NotoSansJP-Bold.japan-subset.otf
//! ```
//!
//! The tile endpoint defaults to OpenFreeMap's planet build, the same
//! public source the browser page defaults to; `--tiles` repoints it.

use gpui_maps::geo::TileId;
use gpui_maps::render::Camera;

/// The endpoint `web/src/app.rs` defaults to (`crate::basemap::OPENFREEMAP_PLANET_TILE_URL`),
/// so both sides read the same bytes unless told otherwise.
const DEFAULT_TILES: &str = gpui_maps::basemap::OPENFREEMAP_PLANET_TILE_URL;

struct Args {
    tiles: String,
    style: String,
    lon: f64,
    lat: f64,
    zoom: f64,
    /// Which zoom to FETCH tiles at, when that is not the display zoom.
    ///
    /// A tile source stops at its own maxzoom and the map OVERZOOMS past it:
    /// the browser keeps asking for the deepest tile the source actually has
    /// and stretches it, while still baking symbols at the display zoom. This
    /// harness fetched at the display zoom unconditionally, which is right
    /// only while the source goes that deep.
    ///
    /// It is not a theoretical gap. `tiles.openfreemap.org`'s planet build
    /// stops at z14 and answers a z17 request with `HTTP 200` and a ZERO-BYTE
    /// body -- not a 404. So a street-zoom run against it decoded six empty
    /// tiles, reported `"features": 0`, and looked for all the world like a
    /// style that matched nothing, while the browser drew the same camera
    /// fully. `None` keeps the old behaviour (tile zoom == display zoom).
    tile_zoom: Option<u8>,
    width: f32,
    height: f32,
    dpr: f32,
    fonts: String,
    sprites: String,
}

fn parse_args() -> Args {
    let mut args = Args {
        tiles: DEFAULT_TILES.to_string(),
        style: "examples/mapbox-style.json".to_string(),
        lon: 139.7671,
        lat: 35.6812,
        zoom: 14.0,
        tile_zoom: None,
        width: 1280.0,
        height: 713.0,
        dpr: 1.0,
        fonts: "system".to_string(),
        // The sheet the browser build ships, by the same base name it
        // fetches -- `sprite::sprite_urls` appends `@2x.json`/`@2x.png`, so
        // this names two files. `none` runs the no-sprite arm.
        sprites: "web/sprites/japan-sprite".to_string(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i + 1 < argv.len() {
        let value = argv[i + 1].clone();
        match argv[i].as_str() {
            "--tiles" => args.tiles = value,
            "--style" => args.style = value,
            "--lon" => args.lon = value.parse().expect("--lon"),
            "--lat" => args.lat = value.parse().expect("--lat"),
            "--zoom" => args.zoom = value.parse().expect("--zoom"),
            "--tile-zoom" => args.tile_zoom = Some(value.parse().expect("--tile-zoom")),
            "--width" => args.width = value.parse().expect("--width"),
            "--height" => args.height = value.parse().expect("--height"),
            "--dpr" => args.dpr = value.parse().expect("--dpr"),
            "--fonts" => args.fonts = value,
            "--sprites" => args.sprites = value,
            other => panic!("unknown flag {other:?}"),
        }
        i += 2;
    }
    args
}

/// The font system this arm shapes with. See the module comment.
fn font_system(spec: &str) -> glyphon::FontSystem {
    if spec == "system" {
        return gpui_maps::font_fallback::new_font_system();
    }
    let mut db = glyphon::fontdb::Database::new();
    for path in spec.split(',') {
        let bytes = std::fs::read(path).unwrap_or_else(|err| panic!("--fonts {path}: {err}"));
        db.load_font_data(bytes);
    }
    assert!(!db.is_empty(), "--fonts {spec}: no face parsed");
    // The locale the browser reported when this comparison was taken.
    // Stated rather than read off this machine, because the locale is the
    // only input Han unification has and two arms that disagree on it are
    // not comparable.
    gpui_maps::font_fallback::new_font_system_from_db("ja-JP".to_string(), db)
}

/// The sprite atlas this arm places icons against, read off the LOCAL disk.
///
/// Not `sprite::fetch_sprite_atlas`: this harness must read exactly the
/// bytes `web/build.sh` copies into `dist/`, so that the browser's answer
/// and this one are two runs over one sheet rather than two sheets that
/// happen to share a name. Everything after the read is the shipped code --
/// `parse_sprite_manifest`, then `slice_atlas` -- which is the same pair
/// `fetch_sprite_atlas` and `app::load_sprites` each call once they have
/// their bytes.
fn sprite_atlas(spec: &str) -> Option<gpui_maps::sprite::SpriteAtlas> {
    if spec == "none" {
        return None;
    }
    let json = std::fs::read(format!("{spec}@2x.json"))
        .unwrap_or_else(|err| panic!("--sprites {spec}@2x.json: {err}"));
    let png = std::fs::read(format!("{spec}@2x.png"))
        .unwrap_or_else(|err| panic!("--sprites {spec}@2x.png: {err}"));
    let manifest = gpui_maps::sprite::parse_sprite_manifest(&json).expect("sprite manifest parses");
    Some(gpui_maps::sprite::slice_atlas(&png, &manifest).expect("sprite sheet matches its manifest"))
}

/// Every tile the camera's cover needs at tile zoom `z`, the same way
/// `web/src/app.rs` chooses them: one integer zoom bucket, every tile the
/// viewport's ground footprint touches.
///
/// `z` is passed rather than taken from the camera because the two diverge
/// the moment a source is overzoomed -- see [`Args::tile_zoom`].
fn cover(camera: &Camera, z: u8) -> Vec<TileId> {
    let scale = 2f64.powi(z as i32);
    let (cx, cy) = gpui_maps::geo::lon_lat_to_tile_xy(camera.center_lon, camera.center_lat, z as f64);
    // Half the viewport in tile-INDEX units at `z`. `TILE_SIZE` CSS px is one
    // tile across at the tile's OWN native zoom -- so when `z` is coarser than
    // the camera's (overzoom), the same viewport spans proportionally FEWER
    // tiles of `z`'s grid, by the overzoom factor. Omitting this term (which
    // this function did while it assumed `z == camera.zoom`) asks for a
    // footprint 2^(camera.zoom - z) times too wide in each axis: at z17
    // displayed off a z14 source that is 8x8 = 64 times the tiles, nearly all
    // of them nowhere near the screen.
    let overzoom = 2f64.powf(camera.zoom - z as f64);
    let half_x = camera.viewport_width_px as f64 / 2.0 / gpui_maps::geo::TILE_SIZE / overzoom;
    let half_y = camera.viewport_height_px as f64 / 2.0 / gpui_maps::geo::TILE_SIZE / overzoom;
    let max = scale as i64 - 1;
    let clamp = |v: f64| (v.floor() as i64).clamp(0, max) as u32;
    let (x0, x1) = (clamp(cx - half_x), clamp(cx + half_x));
    let (y0, y1) = (clamp(cy - half_y), clamp(cy + half_y));
    let mut out = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            out.push(TileId { z, x, y });
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let style_bytes = std::fs::read(&args.style).expect("style file");
    let style = gpui_maps::style_spec::parse_and_compile(&style_bytes).expect("style compiles");

    let camera = Camera {
        center_lon: args.lon,
        center_lat: args.lat,
        zoom: args.zoom,
        pitch: 0.0,
        bearing: 0.0,
        viewport_width_px: args.width,
        viewport_height_px: args.height,
        fov_y: Camera::DEFAULT_FOV_Y,
    };
    let display_zoom = args.zoom.floor() as u8;
    let http = reqwest::Client::new();

    let sprites = sprite_atlas(&args.sprites);
    let mut font_system = font_system(&args.fonts);
    let default_family = if args.fonts == "system" {
        gpui_maps::font_fallback::DEFAULT_LABEL_FAMILY.to_string()
    } else {
        // Whatever the loaded faces call themselves -- the browser build
        // hardcodes `"Noto Sans JP"` for the same reason, and reading it
        // back is how a renamed subset stops being a silent tofu.
        font_system
            .db()
            .faces()
            .next()
            .and_then(|f| f.families.first().map(|(name, _)| name.clone()))
            .expect("a parsed face has a family name")
    };
    // One call, and it is the SAME code the frame loop runs: `label_census`
    // walks the plans through `collect_candidates` -> `basemap_candidate` ->
    // `place_visible_symbols` -> `sort_symbols_for_drawing`, which is what
    // `MapView::paint` and `app::draw_labels` each do around their own
    // retained caches. Nothing here reimplements a placement rule.
    // The source's own deepest zoom when it is shallower than the display
    // zoom, so the fetch asks for tiles that EXIST while `display_zoom` keeps
    // baking symbols where the camera really is -- what the browser does. See
    // `Args::tile_zoom`.
    let tile_zoom = args.tile_zoom.unwrap_or(display_zoom);
    let tiles = cover(&camera, tile_zoom);
    let mut plans: Vec<(TileId, gpui_maps::style::SymbolPlan)> = Vec::new();
    for tile in &tiles {
        let url = args
            .tiles
            .replace("{z}", &tile.z.to_string())
            .replace("{x}", &tile.x.to_string())
            .replace("{y}", &tile.y.to_string());
        let response = http.get(&url).send().await.expect("tile fetch");
        if !response.status().is_success() {
            eprintln!("{url}: HTTP {}", response.status());
            continue;
        }
        let bytes = response.bytes().await.expect("tile body");
        let decoded = gpui_maps::mvt::decode(bytes.to_vec()).expect("tile decodes");
        plans.push((
            *tile,
            gpui_maps::style::match_tile_symbols(
                &decoded,
                &style,
                display_zoom,
                gpui_maps::symbol_line::tile_units_per_px(display_zoom, tile.z),
            ),
        ));
    }

    let census = gpui_maps::symbol_pass::label_census(
        &camera,
        &style.layers,
        plans.iter().map(|(tile, plan)| (*tile, plan)),
        args.dpr,
        &mut font_system,
        &default_family,
        sprites.as_ref(),
    );

    // One JSON object, the same field names `map_labels_json` uses, so the
    // two can be diffed without a translation step.
    println!(
        "{}",
        serde_json::json!({
            "arm": args.fonts,
            "sprites": args.sprites,
            "sprite_entries": sprites.as_ref().map_or(0, |atlas| atlas.names().count()),
            "family": default_family,
            "tiles": tiles.len(),
            "features": census.features,
            "candidates": census.candidates,
            "placed": census.placed,
            "labels": census.texts.len(),
            "shaped_total": census.shaped,
            "texts": census.texts,
            "icons": census.icons,
            "missing_icons": census.missing_icons,
            // The discriminator between "this layer never OFFERED a label"
            // and "it offered one and `place_glyphs_along` refused it". Those
            // two look identical from the outside and want opposite fixes,
            // and reading it settled the Liberty road-label bug in one line:
            // Liberty refused 9 where a second OpenMapTiles style over the
            // same tiles refused 11, so the style refusing MORE still placed
            // every road name -- which ruled out refusal entirely and
            // pointed at collision instead. (That second style has since
            // been split out of this repository, which is why it is not
            // named; the measurement stands.)
            "line_labels_refused": census.line_labels_refused,
        })
    );
}

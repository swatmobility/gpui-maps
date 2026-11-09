//! **The keyless demo: OpenFreeMap vector tiles, and an optional imagery
//! basemap under them.**
//!
//! ```text
//! cargo run --release --example basic_map                      # the vector map, as it always was
//! GPUI_MAP_STYLE=satellite cargo run --release --example basic_map
//! ```
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `GPUI_MAP_STYLE` | `liberty` | `liberty` or `satellite` (`examples/satellite-style.json`) |
//! | `GPUI_MAP_RASTER` | per style | `mapbox-satellite`, `s2cloudless`, `natural-earth`, `none` |
//! | `GPUI_MAP_LON`/`LAT`/`ZOOM` | Singapore, z12 | the opening camera |
//!
//! # Which imagery, and why the default needs a token it does not require
//!
//! `GPUI_MAP_STYLE=satellite` defaults to **Mapbox satellite**, because
//! resolution at city zoom is what the imagery is being judged on: at z10
//! over Singapore it resolves the reservoirs, the container port, Sentosa
//! and individual ships, and s2cloudless -- capped near 10 m/px -- cannot.
//! At z6 the two are close, with s2cloudless the darker of the pair and
//! carrying a mosaic seam in its own source data.
//!
//! Mapbox satellite needs `MAPBOX_ACCESS_TOKEN` (from the environment, or
//! `~/.config/gpui-map/mapbox.env` at mode 600). **With no token the app
//! still runs**: it falls back to s2cloudless (the 2016 layer, CC BY 4.0, keyless) and
//! says so once on stderr. Nothing here ever prints the token, and the
//! only thing that sees the assembled url is `HttpTileSource`.
//!
//! # Why the imagery style is a separate document
//!
//! Because a vector water polygon paints over the imagery it is meant to
//! sit under. `examples/satellite-style.json` therefore carries NO fill
//! and NO line layer -- only the raster layer and labels -- and says so in
//! its own `metadata`. Switching imagery on over Liberty is still useful
//! and still works (`s`): Liberty declares a Natural Earth relief layer up
//! to zoom 7, so below that band the relief shows through Liberty's
//! landcover at the style's own `raster-opacity` ramp, and above it the
//! style says not to draw imagery and none is drawn.
//!
//! # Credits, and where fetched tiles go
//!
//! OpenFreeMap's planet tiles are built from OpenStreetMap (ODbL); EOX's
//! Sentinel-2 cloudless 2016 is CC BY 4.0 (the later layers are CC BY-NC-SA and are not used); Mapbox satellite is licensed and
//! credits `(c) Mapbox (c) OpenStreetMap`. **Whichever imagery is live is
//! the one credited**, beside the vector source's own credit and not
//! instead of it -- the engine draws both, see
//! `MapView::frame_attribution`, and `TileSource::attribution` is what
//! makes the credit follow the source rather than a constant in this file.
//!
//! Fetched tiles go through the engine's opt-in HTTP disk cache
//! (`HttpTileSource::with_disk_cache`), as `kit::with_tile_cache` decides
//! from `GPUI_MAP_TILE_CACHE`: the openly licensed sources by default,
//! Mapbox only when asked for (`on`). Two properties, both load-bearing:
//!
//!  - **Nothing lands in the repository.** The cache is
//!    `~/.cache/gpui-map/http-tiles`; Mapbox tiles are licensed data and
//!    redistributing them is a breach, so they must never be committed.
//!  - **One source's bytes can never be read back as another's.** An entry
//!    is keyed on its full URL (host, path, `@2x`, layer year), not on
//!    `z-x-y`, so s2cloudless JPEG is never handed to a reader expecting
//!    Mapbox JPEG.

use std::time::{Duration, Instant};

use gpui::{App, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::window;
use window::demo_window_bounds;

use swat_maps_kit::shell;

use swat_maps_kit::agentic;

use swat_maps_kit::kit;

// The map itself -- styles, imagery, camera, sprites -- is the panel the
// gallery also shows (`basic`). This file is its standalone window.
use swat_maps_kit::panels::basic_map as panel;

fn main() {
    println!("Controls:");
    println!("  drag                 pan");
    println!("  scroll               zoom (toward the cursor)");
    println!("  double-click         zoom in");
    println!("  Shift+drag           rotate (drag sideways) or tilt (drag up for 3D)");
    println!("  Option+scroll        rotate");
    println!("  Shift+Left / Right   rotate 15 degrees");
    println!("  Shift+Up / Down      tilt 10 degrees");
    println!("  N                    reset to north");
    println!("  Shift+N              reset to north and level");
    println!("  D                    tile-status debug overlay (off by default)");
    println!("  T                    day/night terminator (visible below zoom 6)");
    println!("  S                    satellite mode (imagery style + imagery) / vector style");
    println!();
    println!("The debug overlay draws one translucent rectangle per visible tile.");
    println!("Its FILL says what is known about that tile's own data (loaded, decoded");
    println!("but not drawable, pending, absent, failed, unknown); its BORDER says where");
    println!("the pixels actually came from this frame (its own mesh, an ancestor");
    println!("magnified up -- the label carries the zoom distance -- a descendant set,");
    println!("or nothing). It has its own legend. Set GPUI_MAP_DEBUG_TILES=1 to have it");
    println!("on from the very first frame, which is what a capture run wants.");

    if let Ok(other) = std::env::var("GPUI_MAP_STYLE") {
        assert!(
            matches!(other.as_str(), "satellite" | "liberty"),
            "GPUI_MAP_STYLE={other:?} is not one of: liberty, satellite"
        );
    }

    // `--pan --quit-after SECS`: `scripts/gallery-parity.sh`'s windowed
    // check, through `kit::pan_task`/`kit::PanGate` -- the same flags the
    // gallery reads (`examples/studio/cli.rs`), parsed by hand here since
    // this is the only pair this standalone example needs. Unrecognized
    // flags are ignored rather than rejected: this main has never parsed
    // its own argv before, and a future caller's flag should not become a
    // hard error just because this example does not act on it.
    let mut pan = false;
    let mut quit_after: Option<f64> = None;
    {
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            match flag.as_str() {
                "--pan" => pan = true,
                "--quit-after" => {
                    let v = args.next().expect("--quit-after needs a value");
                    quit_after = Some(v.parse().expect("--quit-after wants seconds"));
                }
                _ => {}
            }
        }
    }

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    // The environment, and every way it can be wrong, checked HERE in
    // `main` -- a bad `GPUI_MAP_RASTER`, `mapbox-satellite` with no token, a
    // style that does not compile -- so the run stops with the reason
    // before the app starts, as it always did, rather than inside AppKit's
    // launch callback (where a panic aborts instead of unwinding).
    let bounds = demo_window_bounds();
    let env = kit::PanelEnv::from_process(
        kit::Host::Standalone { title: format!("Basic Map [build {}]", env!("GPUI_MAPS_BUILD_NUMBER")) },
        handle,
        (bounds.size.width.to_f64(), bounds.size.height.to_f64()),
    );
    if let Err(err) = panel::preflight(&env) {
        panic!("{err:#}");
    }

    // The sprite sheet is no longer fetched here, blocking, before the
    // window opens: `kit::spawn_sprites` (called by the panel) fetches it
    // off the UI thread, and `MapView::set_sprites` stashes a sheet that
    // lands before the GPU is up. Icons now arrive after the first frame.
    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();

        // The agentic HTTP API: OFF unless `GPUI_MAP_AGENTIC_PORT`
        // is set, bound to 127.0.0.1 only, and an example rather than the
        // library. Added here for the same reason `mapbox_standard_dark`
        // has it: `GET /map/state` answers "what is this map actually
        // doing" -- the paint census first, then the cover -- so a
        // renderer can be graded without a screenshot, and `POST
        // /map/camera` can drive it to a zoom (past 12, say, where the
        // projection is flat Mercator) without a human at the keyboard.
        //
        // The tile url is recorded verbatim. It carries no token: this
        // example's vector source is OpenFreeMap, which is keyless.
        let mut pending_debug_calls = agentic::spawn(agentic::Provenance {
            style_name: "basic_map".to_string(),
            style_source: panel::style_file().to_string(),
            tile_url: gpui_maps::basemap::OPENFREEMAP_PLANET_TILE_URL.to_string(),
            example: "basic_map",
            build: env!("GPUI_MAPS_BUILD_NUMBER"),
        });

        // Set inside the (non-`move`) window-building closure below, read
        // right after the window opens -- the same pattern
        // `demos/us_block_groups.rs`'s `windowed` uses for `map_entity`.
        let mut pan_map: Option<(gpui::WeakEntity<gpui_maps::map_view::MapView>, (f64, f64, f64))> = None;

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(format!("Basic Map [build {}]", env!("GPUI_MAPS_BUILD_NUMBER")).into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                let built = panel::build(&env, window, cx).expect("basic_map panel");
                if let Some(map) = built.map.as_ref() {
                    pan_map = Some((map.clone(), built.start));
                }
                if let (Some(pending), Some(map)) =
                    (pending_debug_calls.take(), built.map.as_ref().and_then(|m| m.upgrade()))
                {
                    agentic::pump(map, window.window_handle(), pending, cx);
                }
                // The window root is the demo shell, which owns the map
                // and handles the `T` terminator and `S` satellite keys --
                // see `kit/src/shell.rs` for why those keys cannot
                // live in `MapView`.
                built
                    .view
                    .downcast::<shell::DemoShell>()
                    .unwrap_or_else(|_| panic!("basic_map's panel root is a DemoShell"))
            },
        )
        .expect("failed to open window");

        // `--pan --quit-after SECS`: the same shape as
        // `demos/us_block_groups.rs`'s own `--pan --quit-after` -- a
        // scripted drift starting once the first frame has painted, a
        // `kit::PanGate` opened 5 s later (load and first bakes excluded),
        // reported and quit at `quit_after`. Without `--pan`, nothing here
        // runs and the window behaves exactly as it always did.
        if pan {
            if let Some((map, start)) = pan_map {
                cx.spawn(async move |cx| {
                    let started = Instant::now();
                    // Bounded by `quit_after` (or 30s with none): an
                    // occluded or never-frontmost window never paints, and
                    // this must not hang the process forever waiting for a
                    // frame that is never coming (feedback_bound_every_poll_loop).
                    let paint_deadline =
                        quit_after.map(|s| Duration::from_secs_f64(s.max(0.0))).unwrap_or(Duration::from_secs(30));
                    while gpui_maps::render::allocation_stats().draw_calls_total == 0
                        && started.elapsed() < paint_deadline
                    {
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                    cx.update(|cx| kit::pan_task(map, start, cx));
                    let gate_at = Duration::from_secs(5);
                    if started.elapsed() < gate_at {
                        cx.background_executor().timer(gate_at - started.elapsed()).await;
                    }
                    let gate = kit::PanGate::open();
                    if let Some(secs) = quit_after {
                        let target = Duration::from_secs_f64(secs.max(0.0));
                        if started.elapsed() < target {
                            cx.background_executor().timer(target - started.elapsed()).await;
                        }
                        gate.report("pan");
                        println!("quit after {secs}s; peak rss {}", kit::gb(kit::peak_rss_bytes()));
                        cx.update(|cx| cx.quit());
                    }
                })
                .detach();
            }
        } else if let Some(secs) = quit_after {
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs_f64(secs.max(0.0))).await;
                println!("quit after {secs}s; peak rss {}", kit::gb(kit::peak_rss_bytes()));
                cx.update(|cx| cx.quit());
            })
            .detach();
        }
    });

    std::mem::forget(runtime);
}

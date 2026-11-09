//! A map that renders with the network unplugged.
//!
//! The map itself is the panel the gallery also shows (`offline`), in
//! `kit/src/panels/offline.rs`; this file is its standalone window and the
//! up-front `PMTILES_PATH` check below (`eprintln!` + `exit(1)` on a
//! missing or unreadable archive), which the gallery replaces with the
//! missing-data card (`kit::Requirement::EnvPath`) instead of exiting the
//! whole process.
//!
//! Everything the renderer needs comes off the local disk:
//!
//! * **Tiles** from a PMTiles archive, read with
//!   [`gpui_maps::tile_source::pmtiles::PmtilesSource`] — an mmap of one
//!   file, no HTTP client anywhere in the path.
//! * **Sprites** from the sprite cache (`~/.cache/gpui-map/sprites/`,
//!   [`gpui_maps::cached_sprite_atlas`]). OpenFreeMap's sheet is not in the
//!   tree (its licence is unverified), so the FIRST run fetches it once; a
//!   run with no cache and no network draws without icons.
//! * **Style** from the vendored `examples/liberty-style.json`.
//! * **Glyphs** from the system font book. `glyphon::FontSystem::new()`
//!   enumerates installed fonts through fontdb; the style's `"glyphs"` URL is
//!   never dereferenced by this renderer, so text needs no network on any
//!   run, offline or not.
//!
//! Tiles, style and glyphs never reach a socket; the sprite cache does, once.
//!
//! Run it:
//!
//! ```text
//! PMTILES_PATH=singapore.pmtiles cargo run --release --example offline_map
//! ```
//!
//! Prove it, on macOS:
//!
//! ```text
//! sandbox-exec -p '(version 1)(allow default)(deny network*)' \
//!   env PMTILES_PATH=singapore.pmtiles ./target/release/examples/offline_map
//! ```
//!
//! `docs/OFFLINE.md` covers where to get an archive, what a region costs on
//! disk, and what offline does *not* buy you.

use gpui::{App, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::window;
use window::demo_window_bounds;

use swat_maps_kit::shell;

use swat_maps_kit::kit;

use swat_maps_kit::panels::offline as panel;

fn main() {
    let path = match std::env::var(panel::PMTILES_PATH_VAR) {
        Ok(path) if !path.trim().is_empty() => path,
        _ => {
            eprintln!("offline_map: PMTILES_PATH is not set.\n\n{}", panel::HOW_TO_GET_ONE);
            std::process::exit(1);
        }
    };
    if !std::path::Path::new(&path).is_file() {
        eprintln!(
            "offline_map: PMTILES_PATH points at {path}, which is not a readable file.\n\n{}",
            panel::HOW_TO_GET_ONE,
        );
        std::process::exit(1);
    }

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let title = format!("Offline Map [build {}]", env!("GPUI_MAPS_BUILD_NUMBER"));
    let bounds = demo_window_bounds();
    let env = kit::PanelEnv::from_process(
        kit::Host::Standalone { title: title.clone() },
        handle,
        (bounds.size.width.to_f64(), bounds.size.height.to_f64()),
    );

    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
                ..Default::default()
            },
            move |window, cx| {
                let built = panel::build(&env, window, cx).unwrap_or_else(|err| {
                    eprintln!("offline_map: {err:#}");
                    std::process::exit(1);
                });
                built
                    .view
                    .downcast::<shell::FocusedMap>()
                    .unwrap_or_else(|_| panic!("offline_map's panel root is a FocusedMap"))
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
}

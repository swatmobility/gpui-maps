//! A map drawn straight out of a PMTiles archive on a static bucket, with
//! no tile server anywhere in the picture.
//!
//! The map itself is the panel the gallery also shows (`pmtiles-remote`,
//! always Protomaps' Florence demo archive), in
//! `kit/src/panels/pmtiles_remote.rs`; this file is its standalone window
//! plus the `argv[1]`/`argv[2]` URL/style override below, which the gallery
//! panel never reads (see `panels::pmtiles_remote::build`'s doc comment).
//!
//! Run it:
//!
//! ```text
//! cargo run --release --example pmtiles_remote
//! ```
//!
//! That draws Florence out of Protomaps' public demo archive
//! (`https://pmtiles.io/protomaps(vector)ODbL_firenze.pmtiles`, ~6.6 MB on
//! GitHub Pages) over HTTP range requests. Nothing is downloaded up front:
//! opening the archive is one 16 KiB request, and each tile after that is
//! one or two more.
//!
//! Point it somewhere else with a URL and, if that archive uses a different
//! tile schema, a style:
//!
//! ```text
//! cargo run --release --example pmtiles_remote -- https://your-bucket/area.pmtiles
//! cargo run --release --example pmtiles_remote -- https://your-bucket/area.pmtiles examples/liberty-style.json
//! ```
//!
//! A local path works too, which is the point of the archive being one file:
//! the same bytes read the same way whether they are on disk or behind a
//! `Range` header.
//!
//! ```text
//! cargo run --release --example pmtiles_remote -- /path/to/area.pmtiles
//! ```
//!
//! The camera is not hardcoded to Florence -- it is read out of whatever
//! archive is opened (`PmtilesSource::coverage`), so an arbitrary URL opens
//! on its own data rather than on empty ocean.

use gpui::{px, size, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
use gpui_maps::tile_source::pmtiles::DEMO_ARCHIVE_URL;

use swat_maps_kit::shell;

use swat_maps_kit::kit;

use swat_maps_kit::panels::pmtiles_remote as panel;

/// The window this example opens, and the frame the starting zoom is
/// computed against.
const WINDOW_WIDTH: f64 = 1024.0;
const WINDOW_HEIGHT: f64 = 768.0;

fn main() {
    println!("Drag to pan, scroll to zoom (toward cursor), double-click to zoom in, hold Shift+drag to tilt/rotate.");

    let mut args = std::env::args().skip(1);
    let archive = args.next().unwrap_or_else(|| DEMO_ARCHIVE_URL.to_string());
    let style_path = args.next().unwrap_or_else(|| panel::DEFAULT_STYLE.to_string());

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let title = "Remote PMTiles".to_string();
    let env = kit::PanelEnv::from_process(
        kit::Host::Standalone { title: title.clone() },
        handle,
        (WINDOW_WIDTH, WINDOW_HEIGHT),
    );

    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();
        let archive = archive.clone();
        let style_path = style_path.clone();

        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH as f32), px(WINDOW_HEIGHT as f32)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
                ..Default::default()
            },
            move |window, cx| {
                let built = panel::build_with(&env, &archive, &style_path, window, cx)
                    .unwrap_or_else(|err| panic!("failed to open {archive}: {err:#}"));
                built
                    .view
                    .downcast::<shell::FocusedMap>()
                    .unwrap_or_else(|_| panic!("pmtiles_remote's panel root is a FocusedMap"))
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
}

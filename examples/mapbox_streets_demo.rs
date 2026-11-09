//! Mapbox Streets v12 with navigation, in its own window. The map itself is
//! the panel the gallery also shows (`streets`), in
//! `kit/src/panels/streets.rs`; this file is its standalone window.

use gpui::{px, size, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::kit;

use swat_maps_kit::shell;

use swat_maps_kit::panels::streets as panel;

fn main() {
    println!("Drag to pan, scroll to zoom (toward cursor), double-click to zoom in, hold Shift+drag to tilt/rotate.");
    println!("Requires MAPBOX_ACCESS_TOKEN to be set (a Mapbox public 'pk.' token).");
    if std::env::var("DEBUG_CLICK_INSPECT").is_err() {
        println!(
            "tip: set DEBUG_CLICK_INSPECT=1 to print which style layers/features match at each \
             click, and their resolved paint values -- e.g. DEBUG_CLICK_INSPECT=1 MAPBOX_ACCESS_TOKEN=pk.your_token \
             cargo run --example mapbox_streets_demo"
        );
    }

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let title = format!("Mapbox Streets Demo [build {}]", env!("GPUI_MAPS_BUILD_NUMBER"));
    // The token: `MAPBOX_ACCESS_TOKEN`, or `~/.config/gpui-map/mapbox.env`
    // at mode 600 (`kit::mapbox_token`). Checked here, before anything is
    // built, so a missing token stops the example with the reason.
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: title.clone() }, handle, (1024.0, 768.0));
    assert!(
        env.mapbox_token.is_some(),
        "set GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN to run this example (Mapbox is off by default; a Mapbox public 'pk.' token)"
    );

    // The sprite sheet is no longer fetched here, blocking, before the
    // window opens: `kit::spawn_sprites` fetches it off the UI thread, and
    // `MapView::set_sprites` stashes a sheet that lands before the GPU is
    // up. Icons now arrive after the first frame.
    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
                ..Default::default()
            },
            move |window, cx| {
                let built = panel::build(&env, window, cx).expect("mapbox_streets_demo panel");
                built
                    .view
                    .downcast::<shell::FocusedMap>()
                    .unwrap_or_else(|_| panic!("mapbox_streets_demo's panel root is a FocusedMap"))
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
}

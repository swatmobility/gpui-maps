//! SWAT Routes Dark, in its own window. The map itself is the panel in
//! `kit/src/panels/swat_routes_dark.rs` (the gallery no longer lists it:
//! its Basemaps card is Graphite, `kit/src/panels/graphite.rs`); this file is its standalone
//! window, the up-front token check, the `SWAT_ROUTES_DARK_HEADLESS` stop
//! and the skip-cause compile diagnostics (standalone only -- see
//! `panel::print_skip_census`). None of these live in the panel: the
//! gallery never wants them.

use gpui::{App, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::window;
use window::demo_window_bounds;

use swat_maps_kit::shell;

use swat_maps_kit::kit;

use swat_maps_kit::panels::swat_routes_dark as panel;

/// Whether `SWAT_ROUTES_DARK_HEADLESS` asks this run to stop before a
/// window opens. Pure -- an injected `Option<String>`, matching
/// `std::env::var(..).ok()` -- so it is testable without touching the
/// process environment. `Some("")` (the variable set but empty) still
/// counts, matching the original `.is_ok()` check.
fn wants_headless_stop(var: Option<String>) -> bool {
    var.is_some()
}

fn main() {
    println!("Drag to pan, scroll to zoom (toward cursor), double-click to zoom in, hold Shift+drag to tilt/rotate.");
    println!("Requires MAPBOX_ACCESS_TOKEN to be set (a Mapbox public 'pk.' token).");

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let title = format!("SWATroutes Dark Demo [build {}]", env!("GPUI_MAPS_BUILD_NUMBER"));
    let bounds = demo_window_bounds();
    // The token: `MAPBOX_ACCESS_TOKEN`, or `~/.config/gpui-map/mapbox.env`
    // at mode 600. Checked here, before anything is built, so a missing
    // token stops the example with the reason.
    let env = kit::PanelEnv::from_process(
        kit::Host::Standalone { title: title.clone() },
        handle,
        (bounds.size.width.to_f64(), bounds.size.height.to_f64()),
    );
    assert!(
        env.mapbox_token.is_some(),
        "set GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN to run this example (Mapbox is off by default; a Mapbox public 'pk.' token)"
    );

    // The skip-cause breakdown: this style compiled a SECOND time, with
    // stderr captured, purely to read the per-layer skip notices back --
    // see `kit::print_skip_census`'s doc comment. Standalone only, so the
    // gallery's own build never repeats it.
    let style_bytes = kit::asset_bytes(panel::STYLE)
        .unwrap_or_else(|e| panic!("run from the repo root so {} resolves: {e}", panel::STYLE));
    panel::print_skip_census(&style_bytes);

    // Headless verification stop: everything above -- token read, style
    // compile, diagnostics, tile source derivation -- has already run and
    // exercised every real (non-GUI) code path this example has.
    // `SWAT_ROUTES_DARK_HEADLESS` exists so that can be checked without
    // ever opening a window.
    if wants_headless_stop(std::env::var("SWAT_ROUTES_DARK_HEADLESS").ok()) {
        println!("SWAT_ROUTES_DARK_HEADLESS set: stopping before opening a window.");
        std::mem::forget(runtime);
        return;
    }

    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
                ..Default::default()
            },
            move |window, cx| {
                let built = panel::build(&env, window, cx).expect("swat_routes_dark panel");
                built
                    .view
                    .downcast::<shell::FocusedMap>()
                    .unwrap_or_else(|_| panic!("swat_routes_dark's panel root is a FocusedMap"))
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headless_flag_is_only_set_or_not() {
        assert!(!wants_headless_stop(None));
        assert!(wants_headless_stop(Some("1".to_string())));
        assert!(wants_headless_stop(Some(String::new())));
    }
}

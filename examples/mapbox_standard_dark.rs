//! Mapbox Standard Dark, in its own window. The map itself is the panel the
//! gallery also shows (`standard-dark`), in
//! `kit/src/panels/standard_dark.rs`; this file is its standalone window,
//! the up-front token check, the `GPUI_MAP_STYLE` style switch, the
//! `MAPBOX_STANDARD_DARK_HEADLESS` stop, the skip-cause compile diagnostics
//! (standalone only -- see `panel::print_skip_census`), and the
//! dev-harness debug port. None of these live in the panel: the gallery
//! only ever wants the one fixture (`panel::STYLE`), silently.

use gpui::{App, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::window;
use window::demo_window_bounds;

use swat_maps_kit::agentic;

use swat_maps_kit::shell;

use swat_maps_kit::kit;

use swat_maps_kit::panels::standard_dark as panel;

/// Which vendored style `GPUI_MAP_STYLE` names, or `panel::STYLE` (Standard
/// Dark) with none set. Pure -- an injected `Option<&str>`, not
/// `std::env::var` read directly -- so it is testable without touching the
/// process environment. Restores the switch `01f8984` had: `streets` is
/// `mapbox-streets-style.json` (134 layers against the same `composite`
/// source Task 8 measured the globe's silhouette against), `dark-reference`
/// is the reference fixture, `liberty` is OpenFreeMap's own document.
fn resolve_style_file(var: Option<&str>) -> Result<&'static str, String> {
    match var {
        Some("streets") => Ok("examples/mapbox-streets-style.json"),
        Some("dark-reference") => Ok("examples/mapbox-standard-dark-reference-style.json"),
        Some("liberty") => Ok("examples/liberty-style.json"),
        Some(other) => {
            Err(format!("GPUI_MAP_STYLE={other:?} is not one of: streets, dark-reference, liberty (unset = standard dark)"))
        }
        None => Ok(panel::STYLE),
    }
}

/// Whether `MAPBOX_STANDARD_DARK_HEADLESS` asks this run to stop before a
/// window opens, letting every non-GUI code path above (token, compile,
/// diagnostics, tile derivation) run without a GPU. Pure -- an injected
/// `Option<String>`, matching `std::env::var(..).ok()` -- so it is
/// testable without touching the process environment. `Some("")` (the
/// variable set but empty) still counts, matching the original
/// `.is_ok()` check.
fn wants_headless_stop(var: Option<String>) -> bool {
    var.is_some()
}

fn main() {
    println!("Drag to pan, scroll to zoom (toward cursor), double-click to zoom in, hold Shift+drag to tilt/rotate.");
    // `t` is this demo's own key, handled by `shell::DemoShell`. The zoom
    // caveat is stated up front because this example opens at zoom 12,
    // where a correctly working toggle changes nothing on screen: the
    // night floor only reaches the shader through the globe transition,
    // which is 0.0 from zoom 6 up.
    println!(
        "Press T to toggle the day/night terminator (visible below zoom 6, where the globe is); \
         N resets north, C and D are the engine's coverage and tile-status overlays."
    );
    println!("Requires MAPBOX_ACCESS_TOKEN to be set (a Mapbox public 'pk.' token).");

    // The vendored fixture this run compiles: `GPUI_MAP_STYLE` overrides
    // `panel::STYLE` (Standard Dark) with `streets`/`dark-reference`/
    // `liberty`; anything else panics with the ones it does accept. The
    // gallery's own panel never sees this -- see `panel::build_with_style`.
    let style_file = resolve_style_file(std::env::var("GPUI_MAP_STYLE").ok().as_deref()).unwrap_or_else(|e| panic!("{e}"));
    eprintln!("style: {style_file}");

    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let title = format!("Mapbox Standard Dark Demo [build {}]", env!("GPUI_MAPS_BUILD_NUMBER"));
    let bounds = demo_window_bounds();
    // The token: `MAPBOX_ACCESS_TOKEN`, or `~/.config/gpui-map/mapbox.env`
    // at mode 600 (`kit::mapbox_token`). Checked here, before anything is
    // built, so a missing token stops the example with the reason.
    let env = kit::PanelEnv::from_process(
        kit::Host::Standalone { title: title.clone() },
        handle,
        (bounds.size.width.to_f64(), bounds.size.height.to_f64()),
    );
    assert!(
        env.mapbox_token.is_some(),
        "set GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN to run this example (Mapbox is off by default; a Mapbox public 'pk.' token)"
    );

    // The skip-cause breakdown: `style_file` compiled a SECOND time, with
    // stderr captured, purely to read the per-layer skip notices back --
    // see `kit::print_skip_census`'s doc comment. Standalone only, so the
    // gallery's own build (through `kit::compile_style`, silent) never
    // repeats it on every switch to this panel.
    let style_bytes = kit::asset_bytes(style_file)
        .unwrap_or_else(|e| panic!("run from the repo root so {style_file} resolves: {e}"));
    panel::print_skip_census(&style_bytes);

    // Headless verification stop: everything above -- token read, style
    // compile, diagnostics, tile source derivation -- has already run and
    // exercised every real (non-GUI) code path this example has.
    // `MAPBOX_STANDARD_DARK_HEADLESS` exists so that can be checked
    // without ever opening a window.
    if wants_headless_stop(std::env::var("MAPBOX_STANDARD_DARK_HEADLESS").ok()) {
        println!("MAPBOX_STANDARD_DARK_HEADLESS set: stopping before opening a window.");
        std::mem::forget(runtime);
        return;
    }

    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();

        // The agentic HTTP API: OFF unless `GPUI_MAP_AGENTIC_PORT`
        // is set, bound to 127.0.0.1 only. The tile url is recorded
        // WITHOUT the token (`composite_tile_url_template` never reads
        // one). Always the verified `mapbox-streets-style.json` composite,
        // regardless of `style_file` -- see `panel::COMPOSITE_STYLE`'s doc
        // comment.
        let tile_url = panel::composite_tile_url_template().unwrap_or_default();
        let mut pending_debug_calls = agentic::spawn(agentic::Provenance {
            style_name: "Mapbox Standard Dark (vendored fixture)".to_string(),
            style_source: style_file.to_string(),
            tile_url,
            example: "mapbox_standard_dark",
            build: env!("GPUI_MAPS_BUILD_NUMBER"),
        });

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
                ..Default::default()
            },
            move |window, cx| {
                let built = panel::build_with_style(&env, style_file, window, cx).expect("mapbox_standard_dark panel");
                if let (Some(pending), Some(map)) =
                    (pending_debug_calls.take(), built.map.as_ref().and_then(|m| m.upgrade()))
                {
                    agentic::pump(map, window.window_handle(), pending, cx);
                }
                // The window root is the demo shell, which owns the map
                // and handles the `T` terminator toggle -- see
                // `kit/src/shell.rs`.
                built
                    .view
                    .downcast::<shell::DemoShell>()
                    .unwrap_or_else(|_| panic!("mapbox_standard_dark's panel root is a DemoShell"))
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
    fn resolves_the_documented_style_names_and_rejects_the_rest() {
        assert_eq!(resolve_style_file(None), Ok(panel::STYLE));
        assert_eq!(resolve_style_file(Some("streets")), Ok("examples/mapbox-streets-style.json"));
        assert_eq!(
            resolve_style_file(Some("dark-reference")),
            Ok("examples/mapbox-standard-dark-reference-style.json")
        );
        assert_eq!(resolve_style_file(Some("liberty")), Ok("examples/liberty-style.json"));
        let err = resolve_style_file(Some("bogus")).unwrap_err();
        assert!(err.contains("bogus") && err.contains("streets"), "{err}");
    }

    #[test]
    fn the_headless_flag_is_only_set_or_not() {
        assert!(!wants_headless_stop(None));
        assert!(wants_headless_stop(Some("1".to_string())));
        // `std::env::var(..).ok()` on a SET-BUT-EMPTY variable is
        // `Some("")`, matching the original `.is_ok()` check -- an unset
        // variable is the only thing that does not stop the run.
        assert!(wants_headless_stop(Some(String::new())));
    }
}

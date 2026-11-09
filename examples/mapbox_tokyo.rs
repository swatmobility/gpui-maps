//! Mapbox Streets v12 over Tokyo Station, in its own window. The map itself
//! is the panel the gallery also shows (`tokyo`), in
//! `kit/src/panels/tokyo.rs`; this file is its standalone window, plus the
//! `tile_source_label` title suffix (`window::tile_source_label` -- "several
//! hours were lost ... to a render whose source was ambiguous", see that
//! function's doc comment). The gallery never wants a titlebar suffix (it
//! owns the window title itself), so this lives only here.
//!
//! `01f8984`'s own version of this suffix was set correctly in
//! `WindowOptions.titlebar.title` but then clobbered on the very first
//! paint: a SECOND, separately-set and mislabeled
//! `view.set_window_title_prefix("Mapbox Streets Demo ...")` (not Tokyo, no
//! suffix) fed `MapView::paint`'s own per-frame `"{prefix} zoom={:.2}"`
//! rewrite, so the real title was visible for exactly one frame. Here the
//! SAME string backs both `WindowOptions.titlebar.title` and
//! `PanelEnv::Host::Standalone.title` (which `panels::tokyo::build` feeds
//! straight into `set_window_title_prefix`), so the suffix survives every
//! frame instead of only the first.

use gpui::{App, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::window;
use window::demo_window_bounds;

use swat_maps_kit::shell;

use swat_maps_kit::kit;

use swat_maps_kit::panels::tokyo as panel;

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

    // The token, checked here (ahead of everything else) so a missing one
    // stops the example with the reason -- same as every other standalone
    // main. Read directly (not through `PanelEnv`, which is built below
    // once the title needs it) because the tile-origin suffix needs it
    // first.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let token = match kit::mapbox_token(std::env::var(kit::MAPBOX_TOKEN_VAR).ok(), home.as_deref()) {
        Ok(Some(token)) => token,
        Ok(None) => panic!("set GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN to run this example (Mapbox is off by default; a Mapbox public 'pk.' token)"),
        Err(why) => panic!("{why}"),
    };

    // The composite source's tileset list, straight from the style's own
    // `sources.composite.url` -- the same derivation `panels::tokyo::build`
    // makes through `kit::mapbox_composite`, repeated here only because the
    // title has to be ready before the window (and therefore the panel)
    // opens.
    let style_bytes = kit::asset_bytes("examples/mapbox-streets-style.json")
        .expect("run from the repo root so examples/mapbox-streets-style.json resolves");
    let style_json: serde_json::Value =
        serde_json::from_slice(&style_bytes).expect("mapbox-streets-style.json must be valid JSON");
    let tilesets = style_json["sources"]["composite"]["url"]
        .as_str()
        .and_then(|u| u.strip_prefix("mapbox://"))
        .expect("mapbox-streets-style.json must have a mapbox:// sources.composite.url");
    let tile_url = format!("https://api.mapbox.com/v4/{tilesets}/{{z}}/{{x}}/{{y}}.mvt?access_token={token}");
    let tile_origin = window::tile_source_label(&tile_url);
    let title = build_title(&tile_origin);

    let bounds = demo_window_bounds();
    let env = kit::PanelEnv::from_process(
        kit::Host::Standalone { title: title.clone() },
        handle,
        (bounds.size.width.to_f64(), bounds.size.height.to_f64()),
    );
    assert!(
        env.mapbox_token.is_some(),
        "set GPUI_MAP_ALLOW_MAPBOX=1 and MAPBOX_ACCESS_TOKEN to run this example (Mapbox is off by default; a Mapbox public 'pk.' token)"
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
                let built = panel::build(&env, window, cx).expect("mapbox_tokyo panel");
                built
                    .view
                    .downcast::<shell::FocusedMap>()
                    .unwrap_or_else(|_| panic!("mapbox_tokyo's panel root is a FocusedMap"))
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
}

/// The window title, with the tile-origin suffix `01f8984` carried (see
/// the module doc comment on why it now survives past the first frame).
/// Pure -- takes the already-computed label, not a URL -- so it is
/// testable without a token or a network call.
fn build_title(tile_origin: &str) -> String {
    format!("Mapbox Tokyo — {tile_origin} [build {}]", env!("GPUI_MAPS_BUILD_NUMBER"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_carries_the_tile_origin_suffix() {
        let title = build_title("api.mapbox.com");
        assert!(title.starts_with("Mapbox Tokyo — api.mapbox.com ["), "{title}");
        assert!(title.contains(env!("GPUI_MAPS_BUILD_NUMBER")), "{title}");
    }
}

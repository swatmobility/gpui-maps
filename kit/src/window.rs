//! Where a demo window opens.
//!
//! Shared by the demo examples because they are launched over and over --
//! after every landed change -- on the same single display someone is
//! working on. `Bounds::centered` puts each relaunch squarely on top of
//! that work, so the default here is the top-left corner instead.
//!
//! The window still has to be *visible*: macOS stops delivering frames to
//! an occluded window, and a demo that never paints never fetches a tile,
//! which reads as a broken renderer rather than a hidden one. So this
//! moves the window aside; it does not hide it.

use gpui::{point, px, size, Bounds, Pixels};

/// Read an `f32` from the environment, falling back to `default` when the
/// variable is unset or unparseable -- a typo in a demo's placement should
/// move the window, not stop the demo.
fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Top-left of the display by default, overridable per axis with
/// `GPUI_MAP_WINDOW_X`, `_Y`, `_W` and `_H` (logical pixels).
///
/// The default `y` clears the menu bar and the notch: at `0` the titlebar
/// lands underneath them and the window cannot be dragged.
///
/// Two windows side by side is the reason `X` is separately settable --
/// comparing our render against the Mapbox GL JS reference means placing
/// one of them out of the other's way.
pub fn demo_window_bounds() -> Bounds<Pixels> {
    Bounds {
        origin: point(
            px(env_f32("GPUI_MAP_WINDOW_X", 0.0)),
            px(env_f32("GPUI_MAP_WINDOW_Y", 38.0)),
        ),
        size: size(
            px(env_f32("GPUI_MAP_WINDOW_W", 1024.0)),
            px(env_f32("GPUI_MAP_WINDOW_H", 768.0)),
        ),
    }
}

/// A short label naming where the tiles in this window came from.
///
/// The title bar is the only place a screenshot carries provenance. Several
/// hours were lost in this project to a render whose source was ambiguous --
/// a local ClickHouse build and a vendor's CDN look identical once the
/// window is captured, and the conclusions drawn from them are not
/// interchangeable. So the window says which it is.
///
/// Loopback is reported as ClickHouse because that is what serves tiles
/// here: `etl/demo_proxy.py` forwards a single path to a local ClickHouse
/// whose tiles come from `ST_AsMVT`. Anything else is named by its host, so
/// a vendor CDN is never mistaken for our own pipeline.
// `#[path]`-included by several examples; only `mapbox_tokyo` calls it.
#[allow(dead_code)]
pub fn tile_source_label(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("");
    let bare = host.split(':').next().unwrap_or(host);
    match bare {
        "127.0.0.1" | "localhost" | "::1" | "0.0.0.0" => "ClickHouse".to_string(),
        "" => "no source".to_string(),
        other => other.trim_start_matches("www.").to_string(),
    }
}

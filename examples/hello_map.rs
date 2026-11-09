//! The smallest map: OpenFreeMap's Liberty style over its planet tiles, no
//! token, default features only. `cargo run --example hello_map`
use gpui::{prelude::*, px, size, App, Bounds, WindowBounds, WindowOptions};
use gpui_maps::basemaps::{self, Basemap};
use gpui_maps::map_view::MapView;

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let rt = runtime.handle().clone();
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
        cx.open_window(options, move |_window, cx| {
            let b = basemaps::load(Basemap::Liberty).expect("the bundled Liberty style compiles");
            // Singapore, zoom 12; the view resizes itself to the window.
            let map = cx.new(|_| MapView::new(103.85, 1.29, 12.0, 1024.0, 768.0, b.style, b.source, rt.clone()));
            if let Some(sprite) = b.sprite {
                basemaps::spawn_sprites(map.downgrade(), sprite, None, rt, cx);
            }
            map
        })
        .expect("open the window");
    });
    std::mem::forget(runtime); // the app owns the process from here
}

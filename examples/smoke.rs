use gpui::{App, Bounds, WindowBounds, WindowOptions, div, prelude::*, px, size};

struct Smoke;

impl Render for Smoke {
    fn render(&mut self, _window: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        div().size_full().child("gpui-maps smoke test")
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(400.0), px(300.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| Smoke),
        )
        .expect("failed to open window");
    });
}

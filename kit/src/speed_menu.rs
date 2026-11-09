//! The player bar's speed control, as a downstream desktop app draws it
//! (its `panels/vehicle_map.rs`: the `speed-toggle` chip beside
//! play/pause and its `speed-menu`): one chip showing the current speed
//! ("60x"), which opens a small menu of the speeds above it, highest
//! first, the current one marked; a pick sets the speed and closes it.
//!
//! Shared by every Studio page with a docked player bar (the delivery
//! simulation and the bus route variants), so they cannot drift apart.
//!
//! The menu closes on a pick, on a click anywhere outside it (a
//! transparent backdrop under it, over the page, takes that click -- the
//! chip's own included, so a second click on the chip closes it rather
//! than reopening it) and on Escape (the page's key handler calls
//! [`is_escape`]). Opening it moves nothing: it is an absolute overlay,
//! and the chip's box holds the widest speed label from the first frame.

use gpui::prelude::FluentBuilder as _;
use gpui::{div, px, AnyElement, App, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window};
use gpui_component::{v_flex, ActiveTheme as _};

/// Room for "1000x" at `text_sm`, so the chip never resizes with the speed.
pub const TOGGLE_W: f32 = 52.0;

/// The chip: `px_2`, `py_0p5`, `rounded_md`, `text_sm`, a muted hover, the
/// current speed. `id` is its element id and debug selector ("ds-speed");
/// `on_toggle` opens or closes the menu.
pub fn toggle(id: &'static str, label: impl Into<SharedString>, on_toggle: impl Fn(&mut Window, &mut App) + 'static, cx: &App) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .debug_selector(move || id.to_string())
        .w(px(TOGGLE_W))
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_sm()
        .text_center()
        .cursor_pointer()
        .text_color(theme.foreground)
        .hover(|d| d.bg(theme.muted))
        .child(label.into())
        .on_click(move |_, window, cx| on_toggle(window, cx))
}

/// The open menu over a page whose root is `relative`: a transparent
/// backdrop over the whole page (a click on it dismisses) and the menu
/// above the chip at the bar's left, highest speed first. Rows are
/// `"{id}-{i}"` by index into `labels` (lowest speed first, as the
/// pages' `SPEEDS`); the menu is `"{id}-menu"`.
pub fn menu(
    id: &'static str,
    labels: &[String],
    selected: usize,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    on_dismiss: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (bg, border, fg, muted, accent, accent_fg) = (theme.background, theme.border, theme.foreground, theme.muted, theme.accent, theme.accent_foreground);
    let on_pick = std::rc::Rc::new(on_pick);
    let rows = (0..labels.len()).rev().map(|i| {
        let row_id = SharedString::from(format!("{id}-{i}"));
        let on_pick = on_pick.clone();
        div()
            .id(row_id.clone())
            .debug_selector(move || row_id.to_string())
            .px_3()
            .py_1()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .when(i == selected, |d| d.bg(accent))
            .text_color(if i == selected { accent_fg } else { fg })
            .hover(|d| d.bg(muted))
            .child(labels[i].clone())
            .on_click(move |_, window, cx| on_pick(i, window, cx))
    });
    let menu_id = SharedString::from(format!("{id}-menu"));
    let backdrop_id = SharedString::from(format!("{id}-menu-backdrop"));
    div()
        .absolute()
        .inset_0()
        .child(
            div()
                .id(backdrop_id.clone())
                .debug_selector(move || backdrop_id.to_string())
                .absolute()
                .inset_0()
                .occlude()
                // cursor: the menu's dismissing backdrop, not a control
                .on_click(move |_, window, cx| on_dismiss(window, cx)),
        )
        .child(
            v_flex()
                .id("speed-menu")
                .debug_selector(move || menu_id.to_string())
                .occlude()
                .absolute()
                .left_3()
                .bottom_12()
                .p_1()
                .rounded_lg()
                .bg(bg)
                .border_1()
                .border_color(border)
                .shadow_md()
                .children(rows),
        )
        .into_any_element()
}

/// Whether a key press is the Escape that closes an open menu.
#[must_use]
pub fn is_escape(e: &gpui::KeyDownEvent) -> bool {
    e.keystroke.key == "escape"
}

#[cfg(test)]
mod tests {
    use gpui::{div, px, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext};

    struct Host {
        open: bool,
    }

    impl Render for Host {
        fn render(&mut self, _w: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            let labels: Vec<String> = ["1x", "10x", "60x"].iter().map(|s| (*s).to_string()).collect();
            let page = cx.entity().downgrade();
            let bar = div().absolute().left_0().right_0().bottom_0().h(px(44.)).child(super::toggle(
                "speed",
                "60x",
                move |_, cx| {
                    let _ = page.update(cx, |h, cx| {
                        h.open = !h.open;
                        cx.notify();
                    });
                },
                cx,
            ));
            let dismiss = cx.entity().downgrade();
            let menu = self.open.then(|| {
                super::menu(
                    "speed",
                    &labels,
                    2,
                    |_, _, _| {},
                    move |_, cx| {
                        let _ = dismiss.update(cx, |h, cx| {
                            h.open = false;
                            cx.notify();
                        });
                    },
                    cx,
                )
            });
            div().relative().size_full().child(bar).children(menu)
        }
    }

    /// The menu opens above its chip at the bar's left, highest speed
    /// first; it covers the corner (and the brand mark) only while open,
    /// and a click outside closes it.
    #[test]
    fn a_menu_opens_above_its_bar_and_closes_on_an_outside_click() {
        let mut app = TestAppContext::single();
        app.update(gpui_component::init);
        let (host, cx) = app.add_window_view(|_, _| Host { open: false });
        cx.run_until_parked();
        assert!(cx.debug_bounds("speed-menu").is_none(), "closed at open");
        let chip = cx.debug_bounds("speed").expect("the chip");
        cx.simulate_click(chip.center(), gpui::Modifiers::none());
        let menu = cx.debug_bounds("speed-menu").expect("the chip opens the menu");
        assert!(menu.bottom() <= chip.top(), "above the chip");
        let top = |id: &'static str, cx: &mut gpui::VisualTestContext| f32::from(cx.debug_bounds(id).expect("a row").top());
        assert!(top("speed-2", cx) < top("speed-0", cx), "highest first");
        let backdrop = cx.debug_bounds("speed-menu-backdrop").expect("the backdrop");
        cx.simulate_click(gpui::point(backdrop.center().x + px(200.), backdrop.top() + px(40.)), gpui::Modifiers::none());
        assert!(cx.debug_bounds("speed-menu").is_none(), "an outside click closes it");
        assert!(!host.read_with(cx, |h, _| h.open));
    }
}

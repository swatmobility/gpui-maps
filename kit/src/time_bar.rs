//! The standard time bar under a map: play/pause, the speed chip and its
//! menu, the clock, and a long draggable scrubber -- the transit page's
//! player bar (th-app's `vehicle_map.rs` `player_bar`, synced into Studio
//! as `gtfs/th_app/player.rs`), as one component every page with a time
//! axis uses, so they cannot drift apart.
//!
//! The bar is a strip BELOW the map, never over it: the host lays its map
//! out to end [`DRAWN_PX`] above the page's foot and puts the bar there.
//! Its look is th-app's, to the pixel: `px_3 py_2 gap_3`, a top rule, a
//! 20 px play/pause glyph, the speed chip beside it (the other half of "how
//! is this playing"), a fixed-width monospaced clock (neither the
//! scrubber nor the digits move as time passes), then the scrubber with
//! the grab/grabbing hand.
//!
//! What differs per page is data, not code: the ids (agentic routes and
//! tests name them), the speeds and how a speed prints, the clock text
//! (a GTFS service clock "08:22:28 / 26:09:09", a date and time
//! "2026-01-05 08:39:48 / 12:08:00") and its width.
//!
//! Keyboard: with the bar focused (a press anywhere on it focuses it),
//! Space plays or pauses, Left and Right step back and forward (the host
//! decides how far), Escape closes the speed menu ([`key_of`]).

use gpui::prelude::FluentBuilder as _;
use gpui::{div, px, App, CursorStyle, Entity, FocusHandle, InteractiveElement as _, KeyDownEvent, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window};
use gpui_component::slider::{Slider, SliderState};
use gpui_component::{h_flex, v_flex, ActiveTheme as _};

/// The bar's drawn height (`py_2` around the 20 px row, the top rule):
/// what a host keeps free under its map.
pub const DRAWN_PX: f32 = 35.5;

/// th-app's tabular clock face: fixed-advance digits.
pub const CLOCK_FONT: &str = "Menlo";

/// How long after a scrub event the host leaves the slider's value alone
/// (a drag is a stream of `Change`s; `SliderState` reports no start or
/// end). th-app's `SCRUB_GRACE`.
pub const SCRUB_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// The element ids (and debug selectors) of one page's bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ids {
    /// The bar itself.
    pub bar: &'static str,
    pub play: &'static str,
    /// The speed chip.
    pub speed: &'static str,
    /// The scrubber's wrapper.
    pub scrub: &'static str,
    /// The open speed menu.
    pub menu: &'static str,
    /// A speed row is `"{option}-{key}"`, `key` from [`TimeBar::option_key`].
    pub option: &'static str,
}

/// The transit page's ids, th-app's own (its agentic routes and tests use them).
pub const TH_APP_IDS: Ids = Ids { bar: "map-player", play: "play-pause", speed: "speed-toggle", scrub: "map-player-scrub", menu: "speed-menu", option: "speed-option" };

/// A key the bar answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    TogglePlay,
    Back,
    Forward,
    CloseMenu,
}

/// The bar's meaning of a key press, if any.
#[must_use]
pub fn key_of(e: &KeyDownEvent) -> Option<Key> {
    let k = &e.keystroke;
    if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
        return None;
    }
    match k.key.as_str() {
        "space" => Some(Key::TogglePlay),
        "left" => Some(Key::Back),
        "right" => Some(Key::Forward),
        "escape" => Some(Key::CloseMenu),
        _ => None,
    }
}

/// th-app's speed text: `"20x"`, `"0.5x"`.
#[must_use]
pub fn speed_text(speed: f64) -> String {
    if (speed - speed.round()).abs() < 1e-9 {
        format!("{speed:.0}x")
    } else {
        format!("{speed}x")
    }
}

/// th-app's menu row key: the whole speed (`speed-option-20`).
#[must_use]
pub fn whole_key(speed: f64) -> String {
    format!("{}", speed as usize)
}

/// A speed's key as written (`0.5`, `1`, `4`).
#[must_use]
pub fn exact_key(speed: f64) -> String {
    format!("{speed}")
}

/// A date and time clock: `"2026-01-05 08:39:48 / 12:08:00"`, local time,
/// the end as a time of day (the day is the start's).
#[must_use]
pub fn date_time_clock(now_ms: i64, end_ms: i64) -> String {
    let at = |ms: i64, f: &str| chrono::DateTime::from_timestamp_millis(ms).map_or_else(|| "-".to_string(), |d| d.with_timezone(&chrono::Local).format(f).to_string());
    format!("{} / {}", at(now_ms, "%Y-%m-%d %H:%M:%S"), at(end_ms, "%H:%M:%S"))
}

/// One page's bar, this frame.
pub struct TimeBar<'a> {
    pub ids: &'a Ids,
    pub playing: bool,
    pub speed: f64,
    /// The speeds the menu offers, lowest first.
    pub speeds: &'a [f64],
    pub speed_text: fn(f64) -> String,
    pub option_key: fn(f64) -> String,
    /// The clock's text, and the fixed width it is laid out in.
    pub clock: String,
    pub clock_w: f32,
    /// A finger is on the scrubber's thumb (the host's [`SCRUB_GRACE`] test).
    pub dragging: bool,
    pub scrub: &'a Entity<SliderState>,
    /// Takes the keys while the bar has focus; `None`: no keys.
    pub focus: Option<&'a FocusHandle>,
}

impl TimeBar<'_> {
    /// The bar: th-app's `player_bar` layout. `on_play` toggles playing,
    /// `on_speed` opens or closes the menu, `on_key` gets [`key_of`]'s keys.
    pub fn bar(
        &self,
        on_play: impl Fn(&mut Window, &mut App) + 'static,
        on_speed: impl Fn(&mut Window, &mut App) + 'static,
        on_key: impl Fn(Key, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let muted = theme.muted;
        let ids = *self.ids;
        let mut bar = h_flex()
            .id(ids.bar)
            .debug_selector(move || ids.bar.to_string())
            .occlude()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .bg(theme.background)
            .border_t_1()
            .border_color(theme.border);
        if let Some(focus) = self.focus {
            let f = focus.clone();
            bar = bar
                .track_focus(focus)
                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| window.focus(&f, cx))
                .on_key_down(move |e: &KeyDownEvent, window, cx| {
                    if let Some(k) = key_of(e) {
                        cx.stop_propagation();
                        on_key(k, window, cx);
                    }
                });
        }
        bar.child(
            div()
                .id(ids.play)
                .debug_selector(move || ids.play.to_string())
                .w(px(20.))
                .text_color(theme.foreground)
                .cursor_pointer()
                .child(if self.playing { "⏸" } else { "▶" })
                .on_click(move |_, window, cx| on_play(window, cx)),
        )
        .child(
            div()
                .id(ids.speed)
                .debug_selector(move || ids.speed.to_string())
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_sm()
                .cursor_pointer()
                .text_color(theme.foreground)
                .hover(move |d| d.bg(muted))
                .child((self.speed_text)(self.speed))
                .on_click(move |_, window, cx| on_speed(window, cx)),
        )
        .child(div().debug_selector(move || format!("{}-clock", ids.bar)).w(px(self.clock_w)).font_family(CLOCK_FONT).text_xs().text_color(theme.muted_foreground).child(SharedString::from(self.clock.clone())))
        // The grab hand on an overlay above the slider: gpui gives the
        // cursor to the topmost hitbox, and the slider's own track and thumb
        // would otherwise answer "arrow". The overlay registers no
        // listeners, so the drag still reaches the slider.
        .child(
            div()
                .flex_1()
                .relative()
                .debug_selector(move || ids.scrub.to_string())
                .child(Slider::new(self.scrub))
                .child(div().absolute().inset_0().cursor(if self.dragging { CursorStyle::ClosedHand } else { CursorStyle::OpenHand })),
        )
    }

    /// th-app's speed menu, above the chip at the bar's left (absolute in the
    /// host's `relative` region that ends with the bar), highest first, the
    /// current one marked; `on_pick` gets the speed.
    pub fn menu(&self, on_pick: impl Fn(f64, &mut Window, &mut App) + 'static, cx: &App) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let (muted, accent, accent_fg, fg) = (theme.muted, theme.accent, theme.accent_foreground, theme.foreground);
        let ids = *self.ids;
        let on_pick = std::rc::Rc::new(on_pick);
        let key = self.option_key;
        let text = self.speed_text;
        let current = self.speed;
        v_flex()
            .id(ids.menu)
            .debug_selector(move || ids.menu.to_string())
            .occlude()
            .absolute()
            .left_3()
            .bottom_12()
            .p_1()
            .rounded_lg()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .children(self.speeds.iter().rev().map(|&speed| {
                let selected = (current - speed).abs() < 1e-9;
                let sel = SharedString::from(format!("{}-{}", ids.option, key(speed)));
                let on_pick = on_pick.clone();
                div()
                    .id((ids.option, (speed * 100.0) as usize))
                    .debug_selector(move || sel.to_string())
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(accent))
                    .text_color(if selected { accent_fg } else { fg })
                    .hover(move |d| d.bg(muted))
                    .child(text(speed))
                    .on_click(move |_, window, cx| on_pick(speed, window, cx))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_and_clocks_print_as_the_pages_show_them() {
        assert_eq!(speed_text(20.0), "20x");
        assert_eq!(speed_text(0.5), "0.5x");
        assert_eq!(whole_key(100.0), "100");
        assert_eq!(exact_key(0.5), "0.5");
        assert_eq!(exact_key(2.0), "2");
        let c = date_time_clock(0, 3_600_000);
        assert_eq!(c.len(), "1970-01-01 00:00:00 / 01:00:00".len(), "{c}");
    }

    #[test]
    fn the_bar_answers_space_left_right_and_escape_only() {
        let k = |s: &'static str| key_of(&KeyDownEvent { keystroke: gpui::Keystroke::parse(s).unwrap(), is_held: false, prefer_character_input: false });
        assert_eq!(k("space"), Some(Key::TogglePlay));
        assert_eq!(k("left"), Some(Key::Back));
        assert_eq!(k("right"), Some(Key::Forward));
        assert_eq!(k("escape"), Some(Key::CloseMenu));
        assert_eq!(k("cmd-left"), None, "a platform shortcut is not a step");
        assert_eq!(k("a"), None);
    }
}

//! The demo-owned window root that OWNS the map, so an example can bind a
//! key the LIBRARY has no business binding.
//!
//! # Why this file exists
//!
//! `mapbox_standard_dark.rs` used to open its window with the `MapView`
//! itself as the root view, which left an example with nowhere to put a
//! keystroke of its own: the only key handler in the tree was
//! `MapView::paint`'s (`src/map_view.rs:8509`), and that one is library
//! behaviour every consumer of this crate inherits. A sun-terminator switch
//! is not one of MapLibre's keyboard shortcuts — `map_view.rs`'s handler
//! deliberately mirrors those (shift+arrows, N/shift+N) plus this crate's two
//! diagnostic overlays (C, D) — so putting it there would have turned a demo
//! affordance into API surface for the four applications that compile this
//! crate.
//!
//! So the window root is this shell instead: one `div` filling the window,
//! holding the `Entity<MapView>` as its only child, with its own
//! `on_key_down`. Nothing in `src/` changed for it; every call it makes
//! (`MapView::space`, `set_atmosphere`, `set_globe_enabled`,
//! `set_window_title_prefix`, `focus_handle`, `camera`) was already public.
//!
//! # Why the engine's own shortcuts still work: the dispatch path
//!
//! gpui dispatches a key event along exactly ONE path — root to the focused
//! node (`Window::dispatch_key_event`, gpui-0.2.2 `window.rs:3735-3736`),
//! and when NOTHING is focused that path is the root node alone
//! (`focus_node_id_in_rendered_frame`, `window.rs:3982-3990`). Every element
//! gets a dispatch node whose parent is the enclosing element's
//! (`element.rs:419`), so this shell's `div` node is the map node's parent.
//!
//! Two consequences, both load-bearing:
//!
//!  1. **This shell focuses the MAP, never itself.** Focus the shell and the
//!     map's node — a *descendant* of the focused node — drops off the
//!     dispatch path entirely, and shift+arrows / N / C / D stop working.
//!     A demo compositing multiple layers into one presentable app once
//!     tracked focus on its own root div and paid exactly that price; it
//!     bound every key it needed itself.
//!  2. **The map answers first.** The bubble phase walks the path deepest
//!     node first (`window.rs:3896-3906`) and stops at the first
//!     `cx.stop_propagation()`, so `MapView`'s listener sees every keystroke
//!     before this one does and consumes the ones it handled
//!     (`src/map_view.rs:8577-8578`). None of `t`, `g` and `s` is among its
//!     match arms (`map_view.rs:8510-8568`: shift+left/right/up/down,
//!     shift+n, n, unshifted c, unshifted d — verified at `bf38aa8`), so
//!     all three fall through to here and `n` never reaches here at all.
//!
//! This div also deliberately does NOT `track_focus`. A focus-tracked div
//! auto-registers a mouse-down listener that takes focus on any click inside
//! its bounds (`gpui-0.2.2 elements/div.rs:2025-2038`), and this div covers
//! the whole map — so the first click-drag would hand the keyboard to the
//! shell and silently kill case 1 above.
//!
//! # A side effect worth naming: the keyboard reaches this example at all
//!
//! `mapbox_standard_dark.rs` never called `Window::focus`/`FocusHandle::focus`
//! before this shell existed (the library never does either, and says why at
//! `src/map_view.rs:4238-4250`). By `focus_node_id_in_rendered_frame`'s
//! fallback above, that means the map's own key listener was never on the
//! dispatch path: **N, C, D and shift+arrows did nothing in this example**,
//! and neither did `POST /map/key`. `Render::render` below focuses the map's
//! handle, so they work now. That is a fix, not a regression, but it is a
//! behaviour change to state rather than discover.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use gpui::{
    div, px, AnyElement, Context, Entity, InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Styled, Window,
};
use gpui_maps::map_view::MapView;
use gpui_maps::render::atmosphere::Atmosphere;
use gpui_maps::story::timeline::{ActiveCaption, Anchor};
use gpui_maps::tile_source::TileSource;

/// What a live story preview (`POST /story/preview`,
/// `story_agentic::preview_tick`) hands the window root for one frame.
#[allow(dead_code)]
pub struct PreviewFrame {
    /// Captions for a gpui layer over the map this frame. The story
    /// preview hands none since Task 20: the map draws a story's captions
    /// itself, in its story frame (`MapView::set_story_frame`), where the
    /// render places them.
    pub captions: Vec<ActiveCaption>,
    /// Ask for another frame: the story clock is running (or paused at a
    /// settle point, waiting on tiles).
    pub keep_ticking: bool,
}

/// The per-frame hook a story preview installs: called with the map a
/// window root holds, before that map renders, on every frame. `None`
/// means no preview is driving this map.
pub type PreviewHook = fn(&mut MapView, gpui::EntityId, Instant) -> Option<PreviewFrame>;

static PREVIEW_HOOK: OnceLock<PreviewHook> = OnceLock::new();

/// Installs the preview hook. The slot lives here, not in
/// `story_agentic.rs`, because this file is compiled by examples that do
/// not have the agent surface; the first install wins (there is one).
#[allow(dead_code)]
pub fn install_preview_hook(hook: PreviewHook) {
    let _ = PREVIEW_HOOK.set(hook);
}

/// Runs the preview hook for `map`, asks for the next frame while it is
/// playing, and returns the caption layer to draw over the map.
///
/// EVERY window root that holds a native map calls this from its
/// `render`, before the map renders, and adds the result as its last
/// children: `DemoShell`, `FocusedMap`, and each panel with a root of its
/// own (world_dots, census, buildings, race_dots).
/// `gallery_panel_roots_run_the_preview_hook` fails the build of a root
/// that puts a map on screen without it. A root that wraps a `DemoShell`
/// or `FocusedMap` must NOT call it too: the preview would tick twice a
/// frame and its captions draw twice.
pub fn preview_layer(map: &Entity<MapView>, window: &mut Window, cx: &mut gpui::App) -> Option<AnyElement> {
    let hook = *PREVIEW_HOOK.get()?;
    let id = map.entity_id();
    let frame = map.update(cx, |map, _| hook(map, id, Instant::now()))?;
    if frame.keep_ticking {
        window.request_animation_frame();
    }
    (!frame.captions.is_empty()).then(|| caption_layer(&frame.captions))
}

/// Where a caption sits in its full-size layer: (main axis, cross axis)
/// of a column, i.e. (vertical, horizontal), each start/centre/end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    Start,
    Center,
    End,
}

pub fn caption_place(anchor: Anchor) -> (Place, Place) {
    use Place::{Center, End, Start};
    match anchor {
        Anchor::TopLeft => (Start, Start),
        Anchor::Top => (Start, Center),
        Anchor::TopRight => (Start, End),
        Anchor::Left => (Center, Start),
        Anchor::Center => (Center, Center),
        Anchor::Right => (Center, End),
        Anchor::BottomLeft => (End, Start),
        Anchor::Bottom => (End, Center),
        Anchor::BottomRight => (End, End),
    }
}

/// Straight sRGB bytes and a fade alpha as a gpui colour.
pub fn caption_rgba(color: [u8; 4], alpha: f32) -> gpui::Rgba {
    let c = |v: u8| f32::from(v) / 255.0;
    gpui::Rgba { r: c(color[0]), g: c(color[1]), b: c(color[2]), a: c(color[3]) * alpha.clamp(0.0, 1.0) }
}

/// The captions, each in a full-size, transparent, click-through layer
/// over the map, placed at its anchor (24 px from the edge) and nudged by
/// its `offset_px`. No background of its own: an opaque root hides the
/// map's surface.
fn caption_layer(captions: &[ActiveCaption]) -> AnyElement {
    let mut root = div().absolute().top_0().left_0().size_full();
    for caption in captions {
        let (vertical, horizontal) = caption_place(caption.anchor);
        let mut layer = div().absolute().top_0().left_0().size_full().p(px(24.0)).flex().flex_col();
        layer = match vertical {
            Place::Start => layer.justify_start(),
            Place::Center => layer.justify_center(),
            Place::End => layer.justify_end(),
        };
        layer = match horizontal {
            Place::Start => layer.items_start(),
            Place::Center => layer.items_center(),
            Place::End => layer.items_end(),
        };
        let mut text = div()
            .relative()
            .left(px(caption.offset_px.0))
            .top(px(caption.offset_px.1))
            .text_size(px(caption.size_px))
            .text_color(caption_rgba(caption.color, caption.alpha))
            .child(caption.text.clone());
        if caption.plate {
            text = text.px(px(caption.size_px * 0.5)).py(px(caption.size_px * 0.25)).rounded(px(4.0)).bg(caption_rgba([0, 0, 0, 140], caption.alpha));
        }
        root = root.child(layer.child(text));
    }
    root.into_any_element()
}

/// The key that toggles the day/night terminator.
///
/// Unmodified, like the engine's own `C` and `D` view toggles and for the
/// same reason they are: it is something a human already looking at the
/// globe reaches for, which is not a moment to look up a chord.
pub const TERMINATOR_KEY: &str = "t";

/// The key that switches the globe projection on and off
/// (`MapView::set_globe_enabled`).
///
/// Unmodified, beside `t`, and not one of `MapView`'s own arms —
/// `map_view.rs`'s handler matches shift+arrows, shift+n, n, unshifted c
/// and unshifted d, so `g` falls through to this shell exactly as `t` does.
pub const GLOBE_KEY: &str = "g";

/// The key that switches satellite mode on and off: the imagery style
/// plus its raster source, against a vector style.
///
/// Unmodified, beside `t` and `g`, and not one of `MapView`'s own arms —
/// `map_view.rs`'s handler matches shift+arrows, shift+n, n, unshifted c
/// and unshifted d, so `s` falls through to this shell exactly as they do.
pub const SATELLITE_KEY: &str = "s";

/// The imagery basemap a shell can switch, and whether it is on now.
///
/// Held by the shell rather than by the `MapView` because `MapView` has
/// no concept of "a source I am not currently using":
/// `clear_raster_source` drops the source, so something has to remember
/// what to put back. The same shape [`Terminator`] has, for the same
/// reason.
///
/// # Why the example supplies the source and the shell does not build one
///
/// Because the credit is part of the source (`TileSource::attribution`),
/// and a credit is a licence obligation rather than decoration. A shell
/// that built its own imagery source would be choosing a provider on the
/// example's behalf and stating that provider's credit from a file that
/// has no idea which map is being drawn. The example knows both.
pub struct Satellite {
    source: Arc<dyn TileSource>,
    /// The edge length every tile from `source` decodes to, checked by
    /// `raster::decode_tile` rather than assumed. 512 for Mapbox's `@2x`
    /// satellite endpoint and for OpenFreeMap's Natural Earth relief; 256
    /// for EOX's s2cloudless WMTS, which has no 2x variant.
    tile_size: u32,
    /// What to call it in the window title — `mapbox-satellite`,
    /// `s2cloudless`. Shown because at a zoom the style's raster layer
    /// excludes, a correct toggle changes nothing on screen.
    name: String,
    on: bool,
    /// The two styles the toggle switches BETWEEN: `(imagery, vector)`.
    ///
    /// # Why the key switches the style and not just the source
    ///
    /// Because an imagery style has no vector content to fall back on.
    /// `examples/satellite-style.json` deliberately carries no fill and
    /// no line layer — a vector water polygon paints over the imagery it
    /// is meant to sit under — so removing the raster source from it
    /// leaves a document with nothing to draw and the window goes black.
    /// That is an empty style, not a renderer fault, and it is exactly
    /// what a source-only toggle produced.
    ///
    /// Switching the whole style is what Mapbox and Google both do and
    /// what someone pressing a satellite button expects: imagery with
    /// labels, or a real vector map. `None` keeps the source-only
    /// behaviour for an example that has one style, and
    /// [`DemoShell::toggle_satellite`] says so on stderr rather than
    /// leaving a key that empties the map.
    styles: Option<(gpui_maps::style::Style, gpui_maps::style::Style)>,
}

impl Satellite {
    // `#[path]`-included by more than one example, and an example that
    // configures no imagery never calls this — the same dead-code shape
    // `kit/src/window.rs`'s `tile_source_label` has. The allow is
    // on the item rather than the module so a genuinely unused field or
    // method elsewhere in this file still reports.
    #[allow(dead_code)]
    pub fn new(
        source: Arc<dyn TileSource>,
        tile_size: u32,
        name: impl Into<String>,
        on_at_launch: bool,
    ) -> Self {
        Self { source, tile_size, name: name.into(), on: on_at_launch, styles: None }
    }

    /// The two documents the toggle switches between. See [`Self::styles`].
    #[allow(dead_code)]
    #[must_use]
    pub fn with_styles(
        mut self,
        imagery: gpui_maps::style::Style,
        vector: gpui_maps::style::Style,
    ) -> Self {
        self.styles = Some((imagery, vector));
        self
    }
}

/// A panel's own imagery, as its [`Satellite`] configured it, for a HOST
/// that switches imagery on the panel's behalf (the gallery's map options,
/// whose rule is that the panel's own configuration wins over its default).
#[allow(dead_code)] // read by the gallery only
#[derive(Clone)]
pub struct PanelImagery {
    pub source: Arc<dyn TileSource>,
    pub tile_size: u32,
    /// `natural-earth`, `s2cloudless`, `mapbox-satellite`, ...
    pub name: String,
    /// The imagery style the panel's `s` switches to, if it has one.
    pub style: Option<gpui_maps::style::Style>,
}

thread_local! {
    /// Keyed by the map, held weakly: a closed panel's entry is pruned on
    /// the next declaration or lookup.
    static PANEL_IMAGERY: std::cell::RefCell<Vec<(gpui::WeakEntity<MapView>, PanelImagery)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn declare_panel_imagery(map: &Entity<MapView>, satellite: &Satellite) {
    let imagery = PanelImagery {
        source: satellite.source.clone(),
        tile_size: satellite.tile_size,
        name: satellite.name.clone(),
        style: satellite.styles.as_ref().map(|(imagery, _)| imagery.clone()),
    };
    let weak = map.downgrade();
    PANEL_IMAGERY.with(|all| {
        let mut all = all.borrow_mut();
        all.retain(|(m, _)| m.upgrade().is_some() && m.entity_id() != weak.entity_id());
        all.push((weak, imagery));
    });
}

/// The imagery the panel holding `map` configured, if it configured any.
#[allow(dead_code)] // read by the gallery only
pub fn panel_imagery(map: &Entity<MapView>) -> Option<PanelImagery> {
    PANEL_IMAGERY.with(|all| {
        let mut all = all.borrow_mut();
        all.retain(|(m, _)| m.upgrade().is_some());
        all.iter().find(|(m, _)| m.entity_id() == map.entity_id()).map(|(_, i)| i.clone())
    })
}

/// Which `night_brightness` the terminator returns to when it is switched
/// back ON, and the rule for what the next press does.
///
/// A separate, `Window`-free type because it is the whole of the decision
/// and it is the only part of this file that can be tested without opening a
/// window — see the tests at the bottom.
///
/// # Why it remembers a value instead of assuming 0.25
///
/// `GPUI_MAP_NIGHT` (`src/map_view.rs:7762-7766`) decides the night floor a
/// freshly-built `MapView` starts with, and the toggle has to start from
/// whatever it chose. Two cases the default alone cannot cover:
///
///  - `GPUI_MAP_NIGHT=0` — a black night side, which is the truthful one —
///    must come back as 0.0, not as 0.25.
///  - `GPUI_MAP_NIGHT=1` means the terminator is already OFF
///    (`Atmosphere::night_brightness_clamped` at `1.0` is "no ground
///    shading", `src/render/atmosphere.rs:438-440`), so there is no "on"
///    value to remember and the first press has to fall back to
///    `Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS`, read from the constant
///    rather than restated here. Since 2026-09-29 that is also the
///    DEFAULT case: the engine starts every map with the terminator off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Terminator {
    /// The `night_brightness` a press turns the terminator back on with.
    on_value: f32,
}

impl Terminator {
    /// Seeds the toggle from the atmosphere the `MapView` was actually
    /// constructed with — read back off the view, never re-derived from the
    /// environment a second time, so this cannot disagree with what the
    /// shader is being handed.
    pub fn from_initial(initial: Atmosphere) -> Self {
        Self {
            on_value: if Self::is_on(initial) {
                initial.night_brightness
            } else {
                Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS
            },
        }
    }

    /// Whether the night side is being dimmed at all.
    ///
    /// Decided on `night_brightness_clamped`, not the raw field, because
    /// that accessor is what `MapView::paint` hands the shader
    /// (`src/map_view.rs:8697`): a caller who set `1.5` has the terminator
    /// off as far as anything on screen is concerned, and this has to agree.
    pub fn is_on(atmosphere: Atmosphere) -> bool {
        atmosphere.night_brightness_clamped() < 1.0
    }

    /// The `night_brightness` one press moves `atmosphere` to.
    ///
    /// `1.0` (off) exactly, because that is the value
    /// `Atmosphere::night_brightness`'s own doc comment defines as "ground
    /// shading off entirely" — not a large number that happens to clamp
    /// there.
    pub fn next(self, atmosphere: Atmosphere) -> f32 {
        if Self::is_on(atmosphere) {
            1.0
        } else {
            self.on_value
        }
    }
}

/// When the shell gives the map the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusPolicy {
    /// Standalone: every render (see `render`'s comment for why).
    EveryFrame,
    /// Gallery: the first render the handle exists, then only on a click in
    /// the map -- refocusing every frame would steal the keyboard from the
    /// sidebar.
    // Constructed only by the gallery's panels; see `DemoShell::embedded`.
    #[allow(dead_code)]
    Once,
}

impl FocusPolicy {
    pub fn should_focus(self, handle_exists: bool, focused_before: bool) -> bool {
        handle_exists && (self == FocusPolicy::EveryFrame || !focused_before)
    }
}

/// The window root: the map, full-bleed, plus the demo's own key handling.
pub struct DemoShell {
    map: Entity<MapView>,
    keys: DemoKeys,
    focus: FocusPolicy,
    focused_once: bool,
}

/// The demo keys' state and actions -- `t`, `g`, `s` -- for any window
/// root that holds a map: [`DemoShell`]'s, or a demo's own root that draws
/// something over the map (`demos/world_dots.rs`' counter). The root
/// calls [`Self::on_key`] from its `on_key_down`; everything else here is
/// what `DemoShell` always did.
pub struct DemoKeys {
    /// The window-title prefix WITHOUT the terminator state appended — the
    /// example's own name and build number. The state is appended on every
    /// publish, so this stays the one copy of the base string. `None` inside
    /// the gallery, which owns its window's title: nothing is published.
    title: Option<String>,
    terminator: Terminator,
    /// The last globe state this shell published, mirroring
    /// `MapView::globe_enabled`. Kept here only so [`Self::title_prefix`]
    /// can be `&self` — the map remains the authority, and this is seeded
    /// from it rather than assumed to be `true`.
    globe: bool,
    /// The imagery basemap this shell can switch, or `None` for an
    /// example that has none — in which case `s` does nothing and the
    /// title says nothing about it. See [`Satellite`].
    satellite: Option<Satellite>,
}

impl DemoShell {
    /// Wraps `map` and publishes the initial terminator and globe state
    /// into the window title.
    ///
    /// `title` is the prefix the example would otherwise have handed
    /// `MapView::set_window_title_prefix` itself; the shell takes it over
    /// because the state is shown there (see [`Self::publish_title`]).
    // Unused by an example that reaches for `with_satellite` instead;
    // see `Satellite::new`'s note.
    #[allow(dead_code)]
    pub fn new(map: Entity<MapView>, title: impl Into<String>, cx: &mut Context<Self>) -> Self {
        Self::with_satellite(map, title, None, cx)
    }

    /// [`Self::new`], plus an imagery basemap the `s` key switches.
    ///
    /// A second constructor rather than a fourth parameter on `new`,
    /// because `new` has call sites that have nothing to do with imagery
    /// and threading `None` through them would be an edit to prove
    /// nothing changed. Delegating makes it structural: the old entry
    /// point passes `None` in exactly one place.
    ///
    /// A satellite whose `on` is `true` is applied to the map HERE,
    /// before the first frame, so a run that opts into imagery from the
    /// environment does not have to be told to press a key.
    pub fn with_satellite(
        map: Entity<MapView>,
        title: impl Into<String>,
        satellite: Option<Satellite>,
        cx: &mut Context<Self>,
    ) -> Self {
        let keys = DemoKeys::with_satellite(&map, title, satellite, cx);
        Self { map, keys, focus: FocusPolicy::EveryFrame, focused_once: false }
    }

    /// The shell inside the gallery: no window title (the gallery owns it)
    /// and the map focused once rather than every frame, so a click in the
    /// sidebar keeps the keyboard. A click in the map takes it back.
    // Only the gallery's panels embed a shell.
    #[allow(dead_code)]
    pub fn embedded(map: Entity<MapView>, satellite: Option<Satellite>, cx: &mut Context<Self>) -> Self {
        let keys = DemoKeys::with_satellite_titled(&map, None, satellite, cx);
        Self { map, keys, focus: FocusPolicy::Once, focused_once: false }
    }

    /// Demo-owned keys. Runs only for keystrokes `MapView`'s own handler did
    /// not consume — see the module docs on the dispatch path.
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.keys.on_key(&self.map, event, window, cx);
    }
}

impl DemoKeys {
    /// The keys for `map`, with the initial terminator and globe state
    /// published into the window title (`title` is the prefix the example
    /// would otherwise have handed `MapView::set_window_title_prefix`).
    // Unused by an example that reaches for `with_satellite` instead;
    // see `Satellite::new`'s note.
    #[allow(dead_code)]
    pub fn new(map: &Entity<MapView>, title: impl Into<String>, cx: &mut gpui::App) -> Self {
        Self::with_satellite(map, title, None, cx)
    }

    /// [`Self::new`], plus an imagery basemap the `s` key switches. A
    /// satellite whose `on` is `true` is applied to the map HERE, before
    /// the first frame.
    pub fn with_satellite(
        map: &Entity<MapView>,
        title: impl Into<String>,
        satellite: Option<Satellite>,
        cx: &mut gpui::App,
    ) -> Self {
        Self::with_satellite_titled(map, Some(title.into()), satellite, cx)
    }

    /// [`Self::with_satellite`] with the title optional: `None` publishes no
    /// window title at all (the gallery's case).
    pub fn with_satellite_titled(
        map: &Entity<MapView>,
        title: Option<String>,
        satellite: Option<Satellite>,
        cx: &mut gpui::App,
    ) -> Self {
        let atmosphere = map.read(cx).space().atmosphere;
        let globe = map.read(cx).globe_enabled();
        let keys = Self { title, terminator: Terminator::from_initial(atmosphere), globe, satellite };
        if let Some(satellite) = keys.satellite.as_ref() {
            declare_panel_imagery(map, satellite);
        }
        if let Some(satellite) = keys.satellite.as_ref() {
            if satellite.on {
                let (source, tile_size) = (satellite.source.clone(), satellite.tile_size);
                map.update(cx, |map, _cx| map.set_raster_source(source, tile_size));
            }
        }
        if let Some(prefix) = keys.title_prefix(atmosphere) {
            map.update(cx, |map, _cx| map.set_window_title_prefix(prefix));
        }
        keys
    }

    /// `"<name> [build N] terminator=on night=0.25 globe=on"` — the prefix
    /// `MapView::paint` puts the live `zoom=` readout after
    /// (`src/map_view.rs:8148`).
    ///
    /// `globe=` is in the title for the reason `terminator=` is: a
    /// screenshot has to say which mode produced it. This one matters more,
    /// because the projection blend is between zoom 11 and 12 and at most
    /// zooms an on/off flip changes NOTHING on screen — so a picture of a
    /// flat street map is equally consistent with "globe off" and with
    /// "globe on, above the band", and only the title tells them apart.
    fn title_prefix(&self, atmosphere: Atmosphere) -> Option<String> {
        let title = self.title.as_deref()?;
        // `satellite=` for the reason `globe=` and `terminator=` are
        // here: a screenshot has to say which mode produced it. The
        // SOURCE's name rather than a bare on/off, because the imagery
        // sources look nothing alike and a picture has to say which one
        // it is.
        let satellite = match self.satellite.as_ref() {
            None => String::new(),
            Some(satellite) if satellite.on => format!(" satellite={}", satellite.name),
            Some(_) => " satellite=off".to_string(),
        };
        Some(format!(
            "{title} terminator={} night={:.2} globe={}{satellite}",
            if Terminator::is_on(atmosphere) { "on" } else { "off" },
            atmosphere.night_brightness_clamped(),
            if self.globe { "on" } else { "off" },
        ))
    }

    /// Shows the terminator state in the window title, immediately.
    ///
    /// # Why the title and not an on-screen hint
    ///
    /// The title costs no layout, no element, and — unlike a `div` drawn
    /// over the map — no `MapView::set_chrome_above_map(true)`. That call
    /// re-parents the Metal layer BELOW gpui's and makes the whole window
    /// background transparent (`src/map_view.rs:3268-3297`); it is the right
    /// trade for a delivery-fleet demo's 360 px HUD, and the wrong one for a
    /// six-word status line, because the brief for this change is that the
    /// map keeps behaving exactly as it does now.
    ///
    /// # Why it sets the title AND the prefix
    ///
    /// `set_window_title_prefix` alone is not enough. `MapView::paint`
    /// rewrites the title only on a frame where the rounded zoom CHANGED
    /// (`window_title_zoom_shown`, `src/map_view.rs:8145-8150`), so after a
    /// keypress that touches no camera the new prefix would sit unpublished
    /// until the next pan or zoom — a state indicator that lags the state it
    /// indicates is worse than none. So the prefix is set (for every later
    /// frame the engine writes) and the full title is written here once, in
    /// the engine's exact `"{prefix} zoom={:.2}"` shape so the two cannot
    /// disagree mid-session.
    fn publish_title(&self, map: &Entity<MapView>, atmosphere: Atmosphere, window: &mut Window, cx: &mut gpui::App) {
        let Some(prefix) = self.title_prefix(atmosphere) else { return };
        let zoom = map.read(cx).camera().zoom;
        map.update(cx, |map, _cx| map.set_window_title_prefix(prefix.clone()));
        window.set_window_title(&format!("{prefix} zoom={zoom:.2}"));
    }

    /// Flips the night side between the floor `GPUI_MAP_NIGHT` selected and
    /// `1.0`.
    fn toggle_terminator(&mut self, map: &Entity<MapView>, window: &mut Window, cx: &mut gpui::App) {
        let atmosphere = map.read(cx).space().atmosphere;
        let next = Atmosphere { night_brightness: self.terminator.next(atmosphere), ..atmosphere };
        map.update(cx, |map, map_cx| {
            map.set_atmosphere(next);
            map_cx.notify();
        });
        self.publish_title(map, next, window, cx);

        // Announced on stderr for the same reason the engine announces its
        // own C and D toggles (`src/map_view.rs:8554-8558`): a keypress that
        // produced no visible change is otherwise indistinguishable from one
        // that never arrived. And here there is a second, sharper reason to
        // say so — the night floor only reaches the shader through
        // `GlobeParams`, which is gated on the SPACE fade, `0.0` at
        // and above zoom 6 (`render::globe::GLOBE_ZOOM_MAX`). Not the
        // projection's globeness, which stays 1.0 until zoom 11. This example
        // opens at zoom 12, where the correct behaviour of a working toggle
        // is that NOTHING on screen moves, so the transition is printed
        // beside the value rather than left to be guessed at.
        let transition =
            gpui_maps::render::globe::space_transition_for_zoom(map.read(cx).camera().zoom);
        eprintln!(
            "gpui-maps demo: sun terminator {} (night_brightness {:.2}); globe transition {transition:.2}{}",
            if Terminator::is_on(next) { "on" } else { "off" },
            next.night_brightness_clamped(),
            if transition > 0.0 { "" } else { " -- flat Mercator, so this is not visible until zoom < 6" },
        );
    }

    /// Flips the globe projection on and off — `MapView::set_globe_enabled`,
    /// which pins the projection to pure Mercator at every zoom and takes
    /// the backdrop, atmosphere and stars with it.
    fn toggle_globe(&mut self, map: &Entity<MapView>, window: &mut Window, cx: &mut gpui::App) {
        self.globe = !self.globe;
        let enabled = self.globe;
        map.update(cx, |map, map_cx| {
            map.set_globe_enabled(enabled);
            map_cx.notify();
        });
        let atmosphere = map.read(cx).space().atmosphere;
        self.publish_title(map, atmosphere, window, cx);

        // Announced on stderr for the same reason `toggle_terminator` is,
        // and with the same sharpened version of it: the projection blends
        // only between zoom 11 and 12, so above 12 the correct behaviour of
        // a working switch is that nothing at all moves. The projection
        // transition the globe WOULD have had at this camera is therefore
        // printed beside the state, so a press that changed no pixel is
        // still distinguishable from a press that never arrived.
        let zoom = map.read(cx).camera().zoom;
        let would_be = gpui_maps::render::globe::transition_for_zoom(zoom);
        eprintln!(
            "gpui-maps demo: globe {} (projection transition {:.3} at zoom {zoom:.2}){}",
            if enabled { "on" } else { "off" },
            if enabled { would_be } else { 0.0 },
            if would_be > 0.0 {
                ""
            } else {
                " -- this camera is above the z11-z12 blend band, so both modes draw the same frame"
            },
        );
    }

    /// Switches satellite mode on and off: the imagery STYLE and its
    /// raster source together, not the source alone.
    ///
    /// A no-op for an example that configured no [`Satellite`]: the key
    /// is still consumed, but nothing is claimed about a source that does
    /// not exist.
    fn toggle_satellite(&mut self, map: &Entity<MapView>, window: &mut Window, cx: &mut gpui::App) {
        let Some(satellite) = self.satellite.as_mut() else { return };
        satellite.on = !satellite.on;
        let (on, source, tile_size, name) =
            (satellite.on, satellite.source.clone(), satellite.tile_size, satellite.name.clone());
        let styles = self
            .satellite
            .as_ref()
            .and_then(|satellite| satellite.styles.as_ref())
            .map(|(imagery, vector)| if on { imagery.clone() } else { vector.clone() });
        let has_styles = styles.is_some();
        map.update(cx, |map, map_cx| {
            // The STYLE first, then the source. Either order settles to
            // the same frame, but this one never has an instant where an
            // imagery style is mounted with no imagery source — which is
            // the black frame this change exists to remove.
            if let Some(style) = styles {
                map.set_style(style);
            }
            if on {
                map.set_raster_source(source, tile_size);
            } else {
                map.clear_raster_source();
            }
            map_cx.notify();
        });
        let atmosphere = map.read(cx).space().atmosphere;
        self.publish_title(map, atmosphere, window, cx);

        // Announced on stderr for the same reason the other two toggles
        // are: a keypress that produced no visible change is otherwise
        // indistinguishable from one that never arrived.
        let zoom = map.read(cx).camera().zoom;
        eprintln!(
            "gpui-maps demo: satellite {} ({name}) at zoom {zoom:.2}{}",
            if on { "on" } else { "off" },
            match (on, has_styles) {
                (true, _) =>
                    " -- imagery arrives asynchronously; a tile that has not landed shows its \
                     nearest already-loaded ancestor rather than a hole",
                // The case that used to black the map out. Named, so a
                // reader who sees an empty frame knows which half is
                // missing.
                (false, false) =>
                    " -- this example has only one style, so switching imagery off leaves \
                     whatever that style draws without it",
                (false, true) => " -- switched to the vector style",
            },
        );
    }

    /// Demo-owned keys, for the root holding `map` to call from its
    /// `on_key_down`. Runs only for keystrokes `MapView`'s own handler did
    /// not consume — see the module docs on the dispatch path. A key it
    /// handles stops propagating and repaints the root.
    pub fn on_key<T: 'static>(
        &mut self,
        map: &Entity<MapView>,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<T>,
    ) {
        // Command and Control belong to the host's menus, and Option is the
        // map's scroll-rotate modifier; shift is excluded to match the
        // engine's own unshifted `c`/`d` arms, so a future shift+T cannot be
        // eaten here by accident.
        let modifiers = &event.keystroke.modifiers;
        if modifiers.platform || modifiers.control || modifiers.alt || modifiers.function || modifiers.shift {
            return;
        }
        match event.keystroke.key.as_str() {
            TERMINATOR_KEY => self.toggle_terminator(map, window, cx),
            GLOBE_KEY => self.toggle_globe(map, window, cx),
            // Consumed even when this example configured no imagery:
            // `stop_propagation` is about the key belonging to this
            // shell, not about whether the press did anything.
            SATELLITE_KEY => self.toggle_satellite(map, window, cx),
            _ => return,
        }
        cx.notify();
        cx.stop_propagation();
    }
}

impl Render for DemoShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Give the map the keyboard. `Window::focus` returns early when the
        // handle is already focused (`gpui-0.2.2 window.rs:1386-1394`), so
        // calling it every render costs one comparison and no extra frame —
        // which is why this is here rather than in a one-shot
        // `window.on_next_frame` like `examples/basic_map.rs:166-170` uses.
        // Two reasons that matters: the map's `FocusHandle` does not exist
        // until its first `prepaint` (`src/map_view.rs:8068`), so the first
        // render sees `None`; and a window driven entirely by the agentic
        // bridge may not reach an `on_next_frame` callback at all, since it
        // only paints when something asks it to.
        //
        // Inside the gallery the policy is `Once` instead: refocusing every
        // frame would take the keyboard back from the sidebar. A click in
        // the map gives it back, in either policy.
        let handle = self.map.read(cx).focus_handle();
        if self.focus.should_focus(handle.is_some(), self.focused_once) {
            if let Some(handle) = handle {
                window.focus(&handle, cx);
                self.focused_once = true;
            }
        }
        // Weak: the drawn frame keeps this listener until the frame after
        // the next, and a strong handle would keep the map (and its GPU
        // buffers) alive that long after the shell is dropped.
        let map = self.map.downgrade();
        // A running story preview moves the camera before the map renders.
        let captions = preview_layer(&self.map, window, cx);
        div()
            .id("gpui-maps-demo-shell")
            .relative()
            .size_full()
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                if let Some(handle) = map.upgrade().and_then(|map| map.read(cx).focus_handle()) {
                    window.focus(&handle, cx);
                }
            })
            .child(self.map.clone())
            .children(captions)
    }
}

/// The window root: the map, and the one thing a host owes it -- focus.
///
/// gpui routes a key along the path from the root to the FOCUSED node,
/// and with nothing focused that path is the root node alone. The library
/// never takes focus (see `MapView::focus_handle`), so with the bare
/// `MapView` as the root no key -- `G`, `P`, `N`, shift+arrows -- ever
/// reached the map's listener, which sits one node below the root. This
/// root focuses the map on every render: `Window::focus` returns early when
/// the handle already has focus, and the handle only exists after the
/// map's first `prepaint`, so a one-shot focus on the first frame would
/// find nothing to focus. The same thing [`DemoShell`] does, without that
/// shell's own `T`/`G`/`S` bindings -- its `G` is the globe, and a map with
/// navigation on uses `G` to drive a route.
///
/// Inside the gallery it focuses once ([`FocusPolicy::Once`]), for the
/// reason `DemoShell` does. Moved here from `mapbox_streets_demo.rs`.
#[allow(dead_code)]
pub struct FocusedMap {
    map: Entity<MapView>,
    focus: FocusPolicy,
    focused_once: bool,
}

#[allow(dead_code)]
impl FocusedMap {
    pub fn new(map: Entity<MapView>, focus: FocusPolicy) -> Self {
        Self { map, focus, focused_once: false }
    }
}

impl Render for FocusedMap {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let handle = self.map.read(cx).focus_handle();
        if self.focus.should_focus(handle.is_some(), self.focused_once) {
            if let Some(handle) = handle {
                window.focus(&handle, cx);
                self.focused_once = true;
            }
        }
        // A click in the map takes the keyboard back (the `Once` policy's
        // other half; a no-op under `EveryFrame`, which already holds it).
        // Weak, for `DemoShell::render`'s reason.
        let map = self.map.downgrade();
        let captions = preview_layer(&self.map, window, cx);
        div()
            .relative()
            .size_full()
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                if let Some(handle) = map.upgrade().and_then(|map| map.read(cx).focus_handle()) {
                    window.focus(&handle, cx);
                }
            })
            .child(self.map.clone())
            .children(captions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The atmosphere `space_default` builds for a given `GPUI_MAP_NIGHT`
    /// value, without touching the process environment (which is shared by
    /// every test in this binary).
    fn with_night(night_brightness: f32) -> Atmosphere {
        Atmosphere { night_brightness, ..Atmosphere::default() }
    }

    #[test]
    fn the_default_starts_off_and_one_press_turns_it_on() {
        let default = Atmosphere::default();
        assert!(!Terminator::is_on(default), "the plain default: no terminator until asked (2026-09-29)");
        let toggle = Terminator::from_initial(default);
        assert_eq!(toggle.next(default), Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS);
        assert_eq!(toggle.next(with_night(Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS)), 1.0, "and a second press turns it off");
    }

    /// Two presses return to exactly where the environment put it — for
    /// every starting value, including the two ends of the range.
    /// Story preview captions sit where their anchor says: the nine
    /// anchors are nine distinct placements, corners at the corners.
    #[test]
    fn caption_anchors_place_the_caption() {
        use gpui_maps::story::timeline::Anchor;
        let all = [
            Anchor::TopLeft,
            Anchor::Top,
            Anchor::TopRight,
            Anchor::Left,
            Anchor::Center,
            Anchor::Right,
            Anchor::BottomLeft,
            Anchor::Bottom,
            Anchor::BottomRight,
        ];
        let places: std::collections::HashSet<_> = all.iter().map(|a| caption_place(*a)).collect();
        assert_eq!(places.len(), 9);
        assert_eq!(caption_place(Anchor::BottomLeft), (Place::End, Place::Start));
        assert_eq!(caption_place(Anchor::TopRight), (Place::Start, Place::End));
        // The fade multiplies the caption's own alpha.
        let c = caption_rgba([255, 0, 0, 255], 0.5);
        assert!((c.r - 1.0).abs() < 1e-6 && (c.a - 0.5).abs() < 1e-6);
        assert_eq!(caption_rgba([0, 0, 0, 128], 2.0).a, 128.0 / 255.0, "alpha is clamped to 1");
    }

    /// Review F2 (Task 19): every window root that puts a native map on
    /// screen runs the story-preview hook, or `POST /story/preview`
    /// answers `ok` there and nothing moves. Scans every panel source, so
    /// a new root is caught too.
    #[test]
    fn gallery_panel_roots_run_the_preview_hook() {
        let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
        // The shared panels live in `kit/src/panels`, Studio's own in
        // `examples/panels`.
        let mut files: Vec<std::path::PathBuf> = ["kit/src/panels", "examples/panels"]
            .iter()
            .flat_map(|dir| std::fs::read_dir(root.join(dir)).expect(dir))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
            .collect();
        files.push(root.join("demos/race_dots/panel.rs"));
        files.push(root.join("kit/src/shell.rs"));
        let mut checked = 0;
        for file in files {
            let src = std::fs::read_to_string(&file).unwrap();
            let marker = ["\nimpl Render ", "for "].concat();
            for (at, _) in src.match_indices(&marker) {
                let block = &src[at..];
                let block = &block[..block.find("\n}\n").unwrap_or(block.len())];
                let shows_map = [".child(self.map.clone())", "self.map_child(", "self.map_area("].iter().any(|m| block.contains(m));
                if shows_map {
                    checked += 1;
                    assert!(
                        block.contains(&["preview_", "layer("].concat()),
                        "{}: `{}` shows a map without `shell::preview_layer`",
                        file.display(),
                        block.lines().next().unwrap_or_default()
                    );
                }
            }
        }
        // 5 in the open repository, more in the Studio's (its digital-twin
        // panels are private): a floor that the scan finding nothing fails.
        assert!(checked >= 5, "only {checked} map roots found: the scan is broken");
    }

    #[test]
    fn two_presses_are_the_identity() {
        for start in [0.0_f32, 0.25, 0.5, 1.0] {
            let initial = with_night(start);
            let toggle = Terminator::from_initial(initial);
            let after_one = with_night(toggle.next(initial));
            assert_eq!(
                toggle.next(after_one),
                start,
                "GPUI_MAP_NIGHT={start} did not survive a round trip through the toggle"
            );
        }
    }

    /// `GPUI_MAP_NIGHT=0` is the truthful night side, and the toggle has to
    /// give it back rather than substituting the 0.25 legibility floor.
    #[test]
    fn a_black_night_side_comes_back_black() {
        let black = with_night(0.0);
        assert!(Terminator::is_on(black), "0.0 dims the night side to nothing, which is still dimming it");
        let toggle = Terminator::from_initial(black);
        assert_eq!(toggle.next(black), 1.0);
        assert_eq!(toggle.next(with_night(1.0)), 0.0);
    }

    /// Starting OFF (`GPUI_MAP_NIGHT=1`) leaves no "on" value to remember,
    /// so the first press has to reach for the crate's default.
    #[test]
    fn starting_off_falls_back_to_the_crates_own_default() {
        let off = with_night(1.0);
        assert!(!Terminator::is_on(off));
        let toggle = Terminator::from_initial(off);
        assert_eq!(toggle.next(off), Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS);
    }

    /// A value the shader would clamp to 1.0 is already OFF, so a press must
    /// turn it ON rather than "off" it a second time and strand the toggle.
    #[test]
    fn an_out_of_range_value_is_read_the_way_the_shader_reads_it() {
        let over = with_night(1.5);
        assert!(!Terminator::is_on(over), "night_brightness_clamped() is 1.0 here, which is off");
        let toggle = Terminator::from_initial(over);
        assert_eq!(toggle.next(over), Atmosphere::TERMINATOR_NIGHT_BRIGHTNESS);
    }

    /// The two demo keys must not be the same key, and neither may collide
    /// with one of `MapView`'s own arms -- `g` and `t` reach this shell only
    /// because the engine's handler does not consume them
    /// (`map_view.rs`: shift+arrows, shift+n, n, unshifted c, unshifted d).
    /// A future engine binding on either is the failure this notices.
    #[test]
    fn the_demo_keys_are_distinct_and_outside_the_engines_own_bindings() {
        assert_ne!(TERMINATOR_KEY, GLOBE_KEY);
        assert_ne!(TERMINATOR_KEY, SATELLITE_KEY);
        assert_ne!(GLOBE_KEY, SATELLITE_KEY);
        for engine in ["left", "right", "up", "down", "n", "c", "d"] {
            assert_ne!(TERMINATOR_KEY, engine);
            assert_ne!(GLOBE_KEY, engine);
            assert_ne!(SATELLITE_KEY, engine);
        }
    }

    #[test]
    fn embedded_shell_focuses_the_map_once_not_every_frame() {
        // EveryFrame (standalone): refocus whenever a handle exists.
        assert!(FocusPolicy::EveryFrame.should_focus(true, true));
        assert!(!FocusPolicy::EveryFrame.should_focus(false, false));
        // Once (gallery): only the first time the handle exists.
        assert!(FocusPolicy::Once.should_focus(true, false));
        assert!(!FocusPolicy::Once.should_focus(true, true), "must not steal focus back from the sidebar");
        assert!(!FocusPolicy::Once.should_focus(false, false));
    }
}

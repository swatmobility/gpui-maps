//! Mapbox Streets v12 with navigation enabled. Shared by
//! `examples/mapbox_streets_demo.rs` and the gallery (`streets`, `nav-drive`).
//!
//! `configure_nav` reads the PROCESS's argv (`DemoRequest::from_args`), in
//! the gallery too: `cargo run --example studio -- --nav-sim` autostarts a
//! drive when this panel opens. Deliberate, and a constraint on the
//! gallery's own flags (`--open` and friends): they must not reuse
//! `--nav-sim`, `--nav-button` or `--route`.
//!
//! `#![allow(dead_code)]`: this file is included by two crates (the
//! standalone example and the gallery) that each use a different half of
//! it, so some items are dead in one of them.
#![allow(dead_code)]

use gpui::{App, AppContext as _, Entity, IntoElement, Render, Window};
use gpui_maps::map_view::MapView;

use crate::kit;
use crate::shell::{FocusPolicy, FocusedMap};

/// The drive page's own scrubber toggle/panel: split into its own file
/// (not a `crate::` module -- `#[path]`, a sibling include, the same way
/// `examples/mapbox_streets_demo.rs` includes this very file) so this
/// file's own background-scanning test, `the_panel_root_sets_no_opaque_background`,
/// keeps meaning what it says. See that file's own doc comment.
#[path = "drive_scrubber.rs"]
pub mod drive_scrubber;
pub use drive_scrubber::DriveScrubber;

pub const ID: &str = "streets";
pub const STYLE: &str = "examples/mapbox-streets-style.json";

/// What this panel draws without a token: Streets (open), the
/// closest open streets look ([`kit::KeylessStyle::for_mapbox`]). The
/// drive still works keyless -- its route is the committed OSRM fixture.
pub const KEYLESS: kit::KeylessStyle = kit::KeylessStyle::OpenStreets;

/// What the gallery checks before opening this panel: Mapbox Streets with
/// a token, [`KEYLESS`] without one (a `Status::Fallback` banner, never the
/// missing-data card).
pub fn requirements(_env: &kit::PanelEnv) -> Vec<kit::Requirement> {
    vec![kit::Requirement::Network, kit::Requirement::MapboxToken { fallback: Some(KEYLESS.label()) }]
}

/// Mapbox Streets/Terrain/Bathymetry with a token, OpenFreeMap without,
/// plus OSRM and osrm-text-instructions: `G` drives the committed OSRM
/// fixture route with its turn text, in `streets` as in `nav-drive`.
pub const CREDITS: &[kit::Credit] =
    &[kit::CREDIT_MAPBOX, kit::CREDIT_OPENFREEMAP_KEYLESS, kit::CREDIT_OSRM_ROUTE, kit::CREDIT_OSRM_TEXT_INSTRUCTIONS];

/// What a host may force on beyond what the command line asks for.
#[derive(Default)]
pub struct Options {
    /// Start the simulated drive at launch, as `--nav-sim` does.
    pub nav_autostart: bool,
    /// Show the engine-drawn Simulate button, as `--nav-button` does.
    pub show_simulate: bool,
}

pub fn build(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, Options::default(), window, cx)
}

/// The gallery's `nav-drive` entry: this same Mapbox Streets panel, with the
/// demo route autostarted and the Simulate button shown -- as `--nav-sim
/// --nav-button` do for the standalone `streets` example -- so opening it
/// shows turn-by-turn driving immediately rather than a static map.
pub fn build_nav_drive(env: &kit::PanelEnv, window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    build_with(env, Options { nav_autostart: true, show_simulate: true }, window, cx)
}

/// Where the turn-by-turn drive stood when its page was dropped (the user,
/// 2026-10-25: "Leaving the page should pause the drive"). Another Gallery
/// card replacing `nav-drive` drops its map, and with it the drive; the
/// place is kept here and the page opened again drives on from it. A page
/// that only covers the map (Settings, Exports, Home) keeps the map alive
/// and suspends it instead (`MapView::set_nav_suspended`).
#[derive(Clone, Debug, PartialEq)]
pub struct DriveResume {
    pub route: std::path::PathBuf,
    /// Metres along the route (`SimOptions::start_at_m`).
    pub driven_m: f64,
    pub speed: u8,
    /// The user's own pause, kept: a paused drive comes back paused.
    pub paused: bool,
    /// Stopped by the strip's Stop: it comes back stopped.
    pub stopped: bool,
    pub muted: bool,
    pub camera: gpui_maps::nav::host::NavCameraMode,
    pub zoom_offset: f64,
}

static DRIVE_RESUME: std::sync::Mutex<Option<DriveResume>> = std::sync::Mutex::new(None);

fn resume_slot() -> std::sync::MutexGuard<'static, Option<DriveResume>> {
    DRIVE_RESUME.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keeps where `view`'s simulated drive over `route` (`total_m` long)
/// stands, for the page opened again; nothing for an arrived drive, a live
/// trip, or none.
pub fn remember_drive(view: &MapView, route: &std::path::Path, total_m: f64) {
    let point = view.nav_state().and_then(|state| {
        let sim = state.sim?;
        if state.phase == gpui_maps::nav::host::TripPhase::Arrived {
            return None;
        }
        Some(DriveResume {
            route: route.to_path_buf(),
            driven_m: (total_m - state.remaining_distance_m).clamp(0.0, total_m),
            speed: sim.speed,
            paused: view.nav_drive_paused_by_user(),
            stopped: sim.stopped,
            muted: state.voice_muted,
            camera: view.nav_camera_mode(),
            zoom_offset: view.nav_follow_zoom_offset(),
        })
    });
    *resume_slot() = point;
}

/// The place [`remember_drive`] kept for `route`, once.
pub fn take_drive_resume(route: &std::path::Path) -> Option<DriveResume> {
    let mut slot = resume_slot();
    if slot.as_ref().is_some_and(|r| r.route == route) {
        slot.take()
    } else {
        None
    }
}

/// The route's length, metres: the steps' own distances, as the
/// scrubber's axis and `SimOptions::start_at_m` measure it.
fn route_total_m(bytes: &[u8]) -> Option<f64> {
    let route = gpui_maps::nav::route_source::parse_route(bytes).ok()?;
    let total: f64 = route.route.steps.iter().map(|s| s.distance).sum();
    (total > 0.0).then_some(total)
}

pub fn build_with(env: &kit::PanelEnv, opts: Options, _window: &mut Window, cx: &mut App) -> anyhow::Result<kit::Built> {
    // With a token: the real, vendored Mapbox Streets-v12 style JSON over
    // real Mapbox vector tiles, whose composite tileset list (streets-v8,
    // terrain-v2, bathymetry-v2) comes from the style's own
    // `sources.composite.url` -- `landcover` and `hillshade` live only in
    // terrain-v2. Without one: Streets (open) ([`KEYLESS`]).
    let kit::PageBasemap { style, source, sprite, sprite_token, .. } =
        kit::mapbox_or_keyless(STYLE, STYLE, env.mapbox_token.as_deref())?;
    let (w, h) = env.viewport;
    let host = env.host.clone();
    let runtime = env.runtime.clone();
    // Where the map opens. Malaysia-wide by default. But a drive
    // that starts on open opens AT the route's start, at the follow zoom:
    // from Malaysia at z6 the follow camera glided 300 km and ten zoom
    // levels in 800 ms, asking for tiles at every zoom on the way and
    // landing at z16/45 deg on a city with nothing cached -- the user's
    // "opens with just grey, takes a while to load".
    let start = if opts.nav_autostart || nav_request().is_some_and(|d| d.autostart) {
        drive_start().unwrap_or(MALAYSIA)
    } else {
        MALAYSIA
    };
    let map = cx.new(|_| {
        let mut view = MapView::new(start.0, start.1, start.2, w, h, style, source, runtime);
        if let Some(title) = host.title() {
            view.set_window_title_prefix(title.to_string());
        }
        if host.is_gallery() {
            view.set_chrome_above_map(true);
        }
        // This demo is what shows navigation off, so it opts in:
        // right-click-to-route and `G`/`Shift+G`/`P` are OFF on a new
        // `MapView` until this call (see `MapView::set_nav_enabled`, S2 in
        // the turn-by-turn final review).
        view.set_nav_enabled(true);
        configure_nav(&mut view, &opts);
        view
    });
    // The page's drive, dropped with its map (another card opened): its
    // place is kept for the page opened again.
    if opts.nav_autostart {
        let path = route_path(nav_request().as_ref());
        if let Some(total) = std::fs::read(&path).ok().as_deref().and_then(route_total_m) {
            cx.observe_release(&map, move |view, _| remember_drive(view, &path, total)).detach();
        }
    }
    // `resolve_sprite_url` turns `mapbox://sprites/mapbox/streets-v12` into
    // the Styles API sprite URL with the token; see `kit::spawn_sprites`.
    if let Some(sprite) = sprite {
        kit::spawn_sprites(map.downgrade(), sprite, sprite_token, env.runtime.clone(), cx);
    }
    let focus = if env.host.is_gallery() { FocusPolicy::Once } else { FocusPolicy::EveryFrame };
    let root = cx.new(|_| FocusedMap::new(map.clone(), focus));
    // A drive that starts on open gets its scrubber: hidden by default,
    // a round toggle beside the nav HUD shows it (fix/nav-scrubber-placement
    // -- it used to be a `TimeFilter` card drawn over the HUD, which read
    // as a chart left over from the data pages).
    if opts.nav_autostart {
        if let Some(scrubber) = drive_scrubber(&map, cx) {
            let drive = cx.new(|_| DriveRoot { inner: root, scrubber });
            return Ok(kit::Built { view: drive.into(), map: Some(map.downgrade()), start });
        }
    }
    Ok(kit::Built { view: root.into(), map: Some(map.downgrade()), start })
}

/// The nav-drive page: the map and the drive's scrubber toggle/panel
/// over it. Escape closes the panel wherever focus is in this page --
/// `capture_key_down` on the root runs before the focused map's own key
/// handling (the same mechanism `Gallery::on_map_key` uses for its own
/// popover, `examples/studio/app.rs`).
pub struct DriveRoot {
    inner: Entity<FocusedMap>,
    pub scrubber: Entity<DriveScrubber>,
}

impl DriveRoot {
    /// Closes the scrubber panel if it is open. The root's own
    /// `capture_key_down` (below) calls this for Escape; a test calls it
    /// directly, with no window paint needed -- `inner` holds a real
    /// `MapView`, which this harness panics the moment it is PAINTED
    /// (see this file's test module doc), so a `DriveRoot` under test is
    /// built but never drawn.
    fn on_escape(&mut self, cx: &mut gpui::Context<Self>) {
        self.scrubber.update(cx, |s, cx| {
            if s.open {
                s.open = false;
                cx.notify();
            }
        });
    }
}

impl Render for DriveRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        gpui::div()
            .relative()
            .size_full()
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" {
                    this.on_escape(cx);
                }
            }))
            .child(self.inner.clone())
            .child(self.scrubber.clone())
    }
}

/// The planned speed (km/h) along a route of `steps` (`(distance m,
/// duration s)` each, in order) in `bins` equal stretches of its length:
/// the scrubber's bars.
pub fn speed_profile(steps: &[(f64, f64)], bins: usize) -> Vec<f64> {
    let total: f64 = steps.iter().map(|s| s.0).sum();
    let mut out = vec![0.0; bins.max(1)];
    if total <= 0.0 {
        return out;
    }
    let n = out.len();
    for (b, slot) in out.iter_mut().enumerate() {
        let mid = (b as f64 + 0.5) / n as f64 * total;
        let mut at = 0.0;
        for &(d, t) in steps {
            if mid <= at + d {
                *slot = if t > 0.0 { d / t * 3.6 } else { 0.0 };
                break;
            }
            at += d;
        }
    }
    out
}

/// The drive's scrubber: the route's length as the axis, the planned
/// speed as the bars, the head following the simulated car (polled four
/// times a second from `nav_state`). Dragging the head and letting go
/// restarts the drive from there (`SimOptions::start_at_m`), through
/// [`DriveScrubber::seek_to`]. `None` when the route will not load.
fn drive_scrubber(map: &Entity<MapView>, cx: &mut App) -> Option<Entity<DriveScrubber>> {
    let path = route_path(nav_request().as_ref());
    let bytes = std::fs::read(&path).ok()?;
    let route = gpui_maps::nav::route_source::parse_route(&bytes).ok()?;
    let steps: Vec<(f64, f64)> = route.route.steps.iter().map(|s| (s.distance, s.duration)).collect();
    let total: f64 = steps.iter().map(|s| s.0).sum();
    if total <= 0.0 {
        return None;
    }
    let bars = speed_profile(&steps, 24);
    let weak = map.downgrade();
    let seek: std::rc::Rc<dyn Fn(f64, &mut App)> = std::rc::Rc::new(move |metres: f64, cx: &mut App| {
        let _ = weak.update(cx, |view, cx| {
            let options = gpui_maps::nav::host::SimOptions { start_at_m: Some(metres.max(1.0)), ..Default::default() };
            view.set_nav_demo_route(bytes.clone(), options);
            if let Err(err) = view.start_nav_simulation() {
                eprintln!("nav: {err}");
            }
            cx.notify();
        });
    });
    let scrubber = cx.new(|_| DriveScrubber {
        map: map.downgrade(),
        total_m: total,
        bars,
        driven_m: 0.0,
        open: false,
        dragging: false,
        track: std::rc::Rc::default(),
        seek,
    });
    let (weak_scrubber, weak_map) = (scrubber.downgrade(), map.downgrade());
    cx.spawn(async move |cx| loop {
        cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
        let Ok(remaining) = weak_map.read_with(cx, |m, _| m.nav_state().map(|s| s.remaining_distance_m)) else { return };
        let Ok(()) = weak_scrubber.update(cx, |s, cx| {
            if let Some(r) = remaining {
                s.set_driven((total - r).clamp(0.0, total), cx);
            }
        }) else {
            return;
        };
    })
    .detach();
    Some(scrubber)
}

/// The standalone's (and `streets`') opening view.
const MALAYSIA: (f64, f64, f64) = (102.0, 4.0, 6.0);

/// `--nav-sim` and friends, read off the process's argv.
fn nav_request() -> Option<gpui_maps::nav::host::DemoRequest> {
    let args: Vec<String> = std::env::args().collect();
    gpui_maps::nav::host::DemoRequest::from_args(&args)
}

/// The demo route's file: `--nav-route` (a name or a path), else the
/// default fixture.
fn route_path(demo: Option<&gpui_maps::nav::host::DemoRequest>) -> std::path::PathBuf {
    let route = demo.map_or(gpui_maps::nav::host::DEFAULT_ROUTE.to_string(), |d| d.route.clone());
    if route.ends_with(".json") || route.contains('/') {
        std::path::PathBuf::from(&route)
    } else {
        crate::kit::asset(&format!("tests/fixtures/routes/{route}.json"))
    }
}

/// The demo route's first point at the follow camera's zoom: where a drive
/// that starts on open should open the map, so its first frame already
/// asks for the tiles it will draw. `None` if the route will not load (the
/// drive will not start either, and the map keeps its usual view).
fn drive_start() -> Option<(f64, f64, f64)> {
    drive_start_from(&std::fs::read(route_path(nav_request().as_ref())).ok()?)
}

fn drive_start_from(bytes: &[u8]) -> Option<(f64, f64, f64)> {
    let route = gpui_maps::nav::route_source::parse_route(bytes).ok()?;
    let first = route.route.geometry.first()?;
    Some((first.lng, first.lat, gpui_maps::nav::follow::FOLLOW_ZOOM))
}

/// The navigation set-up, moved verbatim from `mapbox_streets_demo.rs`
/// except that `opts` can force the autostart and the Simulate button on.
fn configure_nav(view: &mut MapView, opts: &Options) {
    // `G` drives a committed fixture route; `--nav-sim` starts
    // one at launch (see `gpui_maps::nav::host::DemoRequest`).
    let demo = nav_request();
    let path = route_path(demo.as_ref());
    // The page's drive as its last visit left it (see `DriveResume`).
    let resume = if opts.nav_autostart { take_drive_resume(&path) } else { None };
    let mut options = demo.as_ref().map(|d| d.options).unwrap_or_default();
    if let Some(r) = &resume {
        options.start_at_m = Some(r.driven_m.max(1.0));
        options.speed = r.speed;
        options.paused = r.paused && !r.stopped;
    }
    match std::fs::read(&path) {
        Ok(bytes) => view.set_nav_demo_route(bytes, options),
        Err(err) => eprintln!("nav: no demo route at {}: {err}", path.display()),
    }
    // `--nav-button`: the engine-drawn Simulate button,
    // clicked through the same chrome tap path as End.
    view.set_nav_show_simulate(opts.show_simulate || demo.as_ref().is_some_and(|d| d.show_button));
    // `--nav-camera MODE` / `--nav-zoom-offset Z` (or the presets
    // `--nav-proximity close|standard|far`, +1 / 0 / -1): the camera the
    // drive opens in, as the HUD pill's mode buttons and a zoom while
    // following would set it -- so a windowed check can frame every choice
    // without clicking.
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    if let Some(name) = flag("--nav-camera") {
        match gpui_maps::nav::host::NavCameraMode::from_name(&name) {
            Some(mode) => view.set_nav_camera_mode(mode),
            None => eprintln!("nav: --nav-camera takes follow3d, follow2d, northup2d or overview, got `{name}`"),
        }
    }
    if let Some(name) = flag("--nav-proximity") {
        match gpui_maps::nav::follow::zoom_offset_preset(&name) {
            Some(offset) => view.set_nav_follow_zoom_offset(offset),
            None => eprintln!("nav: --nav-proximity takes close, standard or far, got `{name}`"),
        }
    }
    if let Some(value) = flag("--nav-zoom-offset") {
        match value.parse::<f64>() {
            Ok(offset) if offset.is_finite() => view.set_nav_follow_zoom_offset(offset),
            _ => eprintln!("nav: --nav-zoom-offset takes a number of zoom levels (+1 is a level closer), got `{value}`"),
        }
    }
    if let Some(r) = &resume {
        view.set_nav_camera_mode(r.camera);
        view.set_nav_follow_zoom_offset(r.zoom_offset);
        view.set_nav_voice_muted(r.muted);
    }
    if opts.nav_autostart || demo.as_ref().is_some_and(|d| d.autostart) {
        if let Err(err) = view.start_nav_simulation() {
            eprintln!("nav: {err}");
        }
        // `--nav-stopped`: the drive as the strip's Stop leaves it.
        if args.iter().any(|a| a == "--nav-stopped") || resume.as_ref().is_some_and(|r| r.stopped) {
            view.stop_nav_drive();
        }
    }
    // Live routing (right-click): SWAT_ROUTING_* from the
    // environment or ~/.config/gpui-map/swat-routing.env.
    // The error, when there is one, names the variable and
    // never a value; nothing here prints the config.
    let routing_file = std::env::var("HOME").ok().and_then(|home| {
        std::fs::read_to_string(std::path::Path::new(&home).join(gpui_maps::nav::host::CONFIG_FILE_RELATIVE)).ok()
    });
    view.set_nav_routing(
        gpui_maps::nav::host::RoutingConfig::resolve(&|name| std::env::var(name).ok(), routing_file.as_deref())
            .map_err(|err| err.to_string()),
    );
}

#[cfg(test)]
mod tests {
    /// The user's "turn-by-turn opens grey": a drive started on open opens
    /// the map at the route's first point, at the follow zoom -- never at
    /// the Malaysia-wide z6 the follow camera then glided 300 km from.
    #[test]
    fn a_drive_started_on_open_opens_at_the_routes_start() {
        let bytes = std::fs::read(super::route_path(None)).expect("the default fixture route is committed");
        let (lon, lat, zoom) = super::drive_start_from(&bytes).expect("the fixture parses");
        assert_eq!(zoom, gpui_maps::nav::follow::FOLLOW_ZOOM);
        let (mlon, mlat, mzoom) = super::MALAYSIA;
        assert!(zoom - mzoom >= 8.0, "the start is at street level, not the country view");
        assert!((lon - mlon).abs() > 0.5 || (lat - mlat).abs() > 0.5, "and at the route, not Malaysia's centre: {lon},{lat}");
        assert!(super::drive_start_from(b"not a route").is_none());
    }

    #[test]
    fn the_panel_root_sets_no_opaque_background() {
        // An opaque background call anywhere on the window -> MapView chain
        // hides the map, which draws in a CAMetalLayer behind gpui -- the
        // bug `race_dots` shipped. This one pin covers both gallery entries
        // this file builds: `streets` (`build`) and `nav-drive`
        // (`build_nav_drive`) share the same `build_with` and the same
        // `FocusedMap` root, so one needle over the whole file is enough --
        // there is no second, separately-styled root to miss.
        //
        // The needle is built at runtime (never written as one contiguous
        // literal) so this very assertion does not match itself via
        // `include_str!`.
        let needle = [".", "bg", "("].concat();
        let source = include_str!("streets.rs");
        assert!(!source.contains(&needle), "streets.rs must not set an opaque background on the map chain");
    }

    /// The scrubber's bars: each stretch of the route takes the planned
    /// speed of the step it lies in.
    #[test]
    fn the_speed_profile_follows_the_steps() {
        // 1 km at 36 km/h (100 s), then 3 km at 108 km/h (100 s).
        let bars = super::speed_profile(&[(1000.0, 100.0), (3000.0, 100.0)], 4);
        assert_eq!(bars.len(), 4);
        assert!((bars[0] - 36.0).abs() < 1e-9 && (bars[1] - 108.0).abs() < 1e-9 && (bars[3] - 108.0).abs() < 1e-9, "{bars:?}");
        assert_eq!(super::speed_profile(&[], 3), vec![0.0; 3]);
    }

    // -----------------------------------------------------------------
    // The drive-scrubber toggle and panel (fix/nav-scrubber-placement).
    //
    // Every test here builds a real `MapView` with a real simulated
    // drive (the committed fixture route, entirely local: no network,
    // no Mapbox token) and a real `DriveScrubber` reading it through
    // `MapView::nav_hud_rects`, but NEVER puts the `MapView` itself in
    // a render tree -- `ensure_gpu`/`MapLayer::new` need a real platform
    // window and this harness's does not have one (the same trap
    // `app.rs`'s own `stand_in_map` sidesteps). Only `DriveScrubber`'s
    // plain gpui shapes are ever drawn.
    // -----------------------------------------------------------------

    use gpui::{size, AppContext as _, ParentElement as _, Styled as _, TestAppContext};
    use gpui_maps::map_view::MapView;

    struct ScrubberHost(gpui::Entity<super::DriveScrubber>);
    impl gpui::Render for ScrubberHost {
        fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            gpui::div().relative().size_full().child(self.0.clone())
        }
    }

    /// A real, never-painted `MapView` with the committed fixture route
    /// driving, at `(w, h)`'s logical viewport -- `DriveScrubber` reads
    /// its `nav_hud_rects()` through this, never its GPU state.
    fn simulated_drive_map(app: &mut TestAppContext, w: f32, h: f32) -> gpui::Entity<MapView> {
        simulated_drive_map_with(app, w, h, gpui_maps::nav::host::SimOptions::default())
    }

    fn simulated_drive_map_with(app: &mut TestAppContext, w: f32, h: f32, options: gpui_maps::nav::host::SimOptions) -> gpui::Entity<MapView> {
        let rt = tokio::runtime::Runtime::new().expect("a test runtime");
        let handle = rt.handle().clone();
        std::mem::forget(rt);
        let bytes = std::fs::read(super::route_path(None)).expect("the default fixture route is committed");
        app.update(|cx| {
            cx.new(|_| {
                let style = gpui_maps::style::Style { layers: Vec::new(), sprite: None, sky: Default::default() };
                let tiles = std::sync::Arc::new(gpui_maps::tile_source::EmptyTileSource);
                let mut v = MapView::new(103.85, 1.29, 12.0, f64::from(w), f64::from(h), style, tiles, handle);
                v.set_nav_enabled(true);
                v.set_nav_demo_route(bytes, options);
                v.start_nav_simulation().expect("the fixture route starts a simulated drive");
                v
            })
        })
    }

    /// Two `gpui::Bounds<Pixels>`/`chrome::Rect` rects overlap (half-open,
    /// window points): the bounds-non-overlap tests' one check.
    fn overlaps(a: gpui::Bounds<gpui::Pixels>, b: gpui_maps::nav::chrome::Rect) -> bool {
        let (ax0, ay0) = (f32::from(a.origin.x), f32::from(a.origin.y));
        let (ax1, ay1) = (ax0 + f32::from(a.size.width), ay0 + f32::from(a.size.height));
        ax0 < b.x + b.w && b.x < ax1 && ay0 < b.y + b.h && b.y < ay1
    }

    /// Same check, both sides a painted `Bounds<Pixels>`.
    fn bounds_overlap(a: gpui::Bounds<gpui::Pixels>, b: gpui::Bounds<gpui::Pixels>) -> bool {
        let rect = gpui_maps::nav::chrome::Rect {
            x: f32::from(b.origin.x),
            y: f32::from(b.origin.y),
            w: f32::from(b.size.width),
            h: f32::from(b.size.height),
        };
        overlaps(a, rect)
    }

    /// The core of this fix: at a normal window and a small one, neither
    /// the always-shown toggle nor the opened panel ever overlaps the
    /// HUD's banner, ETA strip or (this page always runs a simulated
    /// drive) its pause/speed tray -- read from the engine's own
    /// `nav_hud_rects`, never re-derived from the chrome's constants.
    #[test]
    fn the_scrubber_never_overlaps_the_hud_at_two_window_sizes() {
        for (w, h) in [(1280.0_f32, 800.0_f32), (480.0_f32, 700.0_f32)] {
            let mut app = TestAppContext::single();
            let map = simulated_drive_map(&mut app, w, h);
            let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
            let (host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
            cx.simulate_resize(size(gpui::px(w), gpui::px(h)));
            cx.run_until_parked();
            let hud = map.read_with(cx, |m, _| m.nav_hud_rects()).expect("an active trip has HUD rects");

            let toggle = cx.debug_bounds("nav-scrubber-toggle").unwrap_or_else(|| panic!("{w}x{h}: the toggle did not draw"));
            for (name, rect) in [("the banner", hud.top), ("the strip", hud.strip), ("the tray", hud.tray)] {
                if let Some(rect) = rect {
                    assert!(!overlaps(toggle, rect), "{w}x{h}: the toggle {toggle:?} overlaps {name} {rect:?}");
                }
            }

            host.update(cx, |h, cx| h.0.update(cx, |s, cx| { s.open = true; cx.notify(); }));
            cx.run_until_parked();
            if let Some(panel) = cx.debug_bounds("nav-scrubber-panel") {
                assert!(!bounds_overlap(panel, toggle), "{w}x{h}: the open panel {panel:?} overlaps its own toggle {toggle:?}");
                for (name, rect) in [("the banner", hud.top), ("the strip", hud.strip), ("the tray", hud.tray)] {
                    if let Some(rect) = rect {
                        assert!(!overlaps(panel, rect), "{w}x{h}: the open panel {panel:?} overlaps {name} {rect:?}");
                    }
                }
            }
        }
    }

    /// Hidden by default; a click on the toggle shows it, and a second
    /// click (or `DriveRoot::on_escape`, below) hides it again.
    #[test]
    fn the_panel_is_hidden_by_default_and_toggles_on_click() {
        let mut app = TestAppContext::single();
        let map = simulated_drive_map(&mut app, 1280.0, 800.0);
        let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
        let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
        cx.run_until_parked();
        assert!(!scrubber.read_with(cx, |s, _| s.open), "closed on open");
        assert!(cx.debug_bounds("nav-scrubber-panel").is_none(), "no panel drawn while closed");
        let toggle = cx.debug_bounds("nav-scrubber-toggle").expect("the toggle always draws over an active trip");
        let center = gpui::point(toggle.origin.x + toggle.size.width / 2.0, toggle.origin.y + toggle.size.height / 2.0);
        cx.simulate_mouse_down(center, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(center, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(scrubber.read_with(cx, |s, _| s.open), "a click opened it");
        assert!(cx.debug_bounds("nav-scrubber-panel").is_some(), "the panel now draws");

        cx.simulate_mouse_down(center, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(center, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(!scrubber.read_with(cx, |s, _| s.open), "a second click closed it");
        assert!(cx.debug_bounds("nav-scrubber-panel").is_none());
    }

    /// `DriveRoot::on_escape` (what its `capture_key_down` calls for
    /// Escape) closes an open panel and does nothing to a closed one --
    /// built but never drawn (see this section's own doc).
    #[test]
    fn escape_closes_an_open_panel() {
        let mut app = TestAppContext::single();
        let map = simulated_drive_map(&mut app, 1280.0, 800.0);
        let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
        let focus = crate::shell::FocusPolicy::Once;
        let (root, cx) = app.add_window_view(|_, cx| {
            let inner = cx.new(|_| crate::shell::FocusedMap::new(map.clone(), focus));
            super::DriveRoot { inner, scrubber }
        });
        cx.run_until_parked();
        root.update(cx, |r, cx| {
            r.scrubber.update(cx, |s, cx| {
                s.open = true;
                cx.notify();
            });
            r.on_escape(cx);
            assert!(!r.scrubber.read(cx).open, "escape closed the open panel");
            r.on_escape(cx);
            assert!(!r.scrubber.read(cx).open, "escape on an already-closed panel is a no-op");
        });
    }

    // -----------------------------------------------------------------
    // The camera-mode buttons (`feat/nav-continuous-zoom`): one tap each,
    // inline in the pill -- they replaced the popover that covered the
    // vehicle.
    // -----------------------------------------------------------------

    const MODE_BUTTONS: [(&str, gpui_maps::nav::host::NavCameraMode); 4] = [
        ("nav-view-mode-follow3d", gpui_maps::nav::host::NavCameraMode::Follow3d),
        ("nav-view-mode-follow2d", gpui_maps::nav::host::NavCameraMode::Follow2d),
        ("nav-view-mode-northup2d", gpui_maps::nav::host::NavCameraMode::NorthUp2d),
        ("nav-view-mode-overview", gpui_maps::nav::host::NavCameraMode::Overview),
    ];

    /// A left click's mouse-down/up pair at `selector`'s own centre, the
    /// same simulated-click path the scrubber's own tests use.
    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let b = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} did not draw"));
        let center = gpui::point(b.origin.x + b.size.width / 2.0, b.origin.y + b.size.height / 2.0);
        cx.simulate_mouse_down(center, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(center, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
    }

    fn camera_mode_name(map: &gpui::Entity<MapView>, cx: &mut gpui::VisualTestContext) -> Option<String> {
        map.read_with(cx, |m, _| m.nav_state().map(|s| format!("{:?}", s.camera_mode)))
    }

    /// The page as Studio shows it: below a 58 px banner and beside a
    /// 260 px sidebar, so window points and the map's own points differ.
    struct OffsetScrubberHost(gpui::Entity<super::DriveScrubber>, f32, f32, f32, f32);
    impl gpui::Render for OffsetScrubberHost {
        fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            let Self(scrubber, left, top, w, h) = self;
            gpui::div().relative().size_full().child(
                gpui::div().absolute().left(gpui::px(*left)).top(gpui::px(*top)).w(gpui::px(*w)).h(gpui::px(*h)).child(scrubber.clone()),
            )
        }
    }

    /// Each mode button switches the mode with ONE tap, mid-drive, and the
    /// map camera really moves to it (frames advanced the way `paint` runs
    /// them) -- from following, and from a camera a pan has just freed.
    /// Nothing opens or closes; the selected button is the filled one.
    #[test]
    fn each_mode_button_switches_the_camera_in_one_tap_mid_drive() {
        use gpui_maps::camera_control::CameraIntent;
        use gpui_maps::nav::host::NavCameraMode;
        for panned in [false, true] {
            let (left, top, w, h) = (260.0_f32, 58.0_f32, 1209.0_f32, 776.0_f32);
            let mut app = TestAppContext::single();
            let map = simulated_drive_map_with(&mut app, w, h, gpui_maps::nav::host::SimOptions { paused: true, ..Default::default() });
            let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
            let (_host, cx) = app.add_window_view(|_, _| OffsetScrubberHost(scrubber.clone(), left, top, w, h));
            cx.simulate_resize(size(gpui::px(left + w), gpui::px(top + h)));
            cx.run_until_parked();
            map.update(cx, |m, _| m.advance_nav_frames_for_test(2_000.0));
            assert_eq!(map.read_with(cx, |m, _| m.nav_camera_mode()), NavCameraMode::Follow3d, "the default");
            // Follow3d first in the cycle is a no-op from the default; go round twice so each is a change.
            for (id, mode) in MODE_BUTTONS.into_iter().skip(1).chain(MODE_BUTTONS) {
                if panned {
                    map.update(cx, |m, _| {
                        m.apply_camera_intent_for_test(CameraIntent::PanBy { dx_px: 80.0, dy_px: 0.0, anchor_px: (600.0, 400.0) });
                        m.advance_nav_frames_for_test(100.0);
                    });
                    assert_eq!(camera_mode_name(&map, cx).as_deref(), Some("Free"), "{id}: the pan freed the camera");
                }
                click(cx, id);
                assert_eq!(map.read_with(cx, |m, _| m.nav_camera_mode()), mode, "{id}: one tap chose it");
                map.update(cx, |m, cx| {
                    m.advance_nav_frames_for_test(1_500.0);
                    cx.notify();
                });
                cx.run_until_parked();
                let camera = map.read_with(cx, |m, _| *m.camera());
                let what = format!("{id} (panned first: {panned})");
                let state = camera_mode_name(&map, cx);
                match mode {
                    NavCameraMode::Overview => {
                        assert_eq!(state.as_deref(), Some("Overview"), "{what}");
                        assert!(camera.pitch.abs() < 1e-3 && camera.bearing.abs() < 1e-3 && camera.zoom < 16.0, "{what}: flat, north-up, out: {camera:?}");
                    }
                    NavCameraMode::NorthUp2d => {
                        assert_eq!(state.as_deref(), Some("Following"), "{what}");
                        assert!(camera.pitch.abs() < 1e-3 && camera.bearing.abs() < 1e-3, "{what}: {camera:?}");
                    }
                    NavCameraMode::Follow2d => {
                        assert_eq!(state.as_deref(), Some("Following"), "{what}");
                        assert!(camera.pitch.abs() < 1e-3, "{what}: {camera:?}");
                    }
                    _ => {
                        assert_eq!(state.as_deref(), Some("Following"), "{what}");
                        assert!((camera.pitch - gpui_maps::nav::follow::FOLLOW_PITCH_RAD).abs() < 1e-3, "{what}: {camera:?}");
                    }
                }
                // The selected button is the filled one, and only it.
                let (quads, scale) = cx.update(|window, _| (window.painted_quads(), window.scale_factor()));
                for (other, other_mode) in MODE_BUTTONS {
                    let bounds = cx.debug_bounds(other).unwrap_or_else(|| panic!("{other} did not draw"));
                    let filled = quad_at(&quads, bounds, scale).is_some_and(|q| q.background.as_solid().is_some_and(|c| c.a > 0.9));
                    assert_eq!(filled, other_mode == mode, "{what}: {other} filled = {filled}");
                }
                assert!(cx.debug_bounds("nav-view-popover").is_none(), "{what}: there is no popover any more");
            }
        }
    }

    /// The user's "it is on top of vehicle - cannot see it": at the
    /// default window, Studio's offset page and narrow ones, in every
    /// follow mode, no HUD rect (`MapView::nav_hud_rects`: banner, strip,
    /// tray) and none of our pill, mode buttons or timeline toggle comes
    /// within a margin of the puck -- nor of the road just ahead of it
    /// (the 80 px above it, where the heading-up modes draw the next
    /// stretch of route).
    #[test]
    fn no_hud_rect_or_mode_button_covers_the_puck_or_the_road_ahead() {
        use gpui_maps::nav::host::NavCameraMode;
        const MARGIN: f32 = 8.0;
        const AHEAD: f32 = 80.0;
        for (w, h) in [(1280.0_f32, 800.0_f32), (1209.0, 776.0), (480.0, 700.0), (360.0, 740.0), (240.0, 700.0)] {
            for mode in [NavCameraMode::Follow3d, NavCameraMode::Follow2d, NavCameraMode::NorthUp2d] {
                let mut app = TestAppContext::single();
                let map = simulated_drive_map_with(&mut app, w, h, gpui_maps::nav::host::SimOptions { paused: true, ..Default::default() });
                let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
                let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
                cx.simulate_resize(size(gpui::px(w), gpui::px(h)));
                cx.run_until_parked();
                map.update(cx, |m, cx| {
                    m.set_nav_camera_mode(mode);
                    cx.notify();
                });
                cx.run_until_parked();
                // The pill reaches the map on the render above; the camera settles after.
                map.update(cx, |m, _| m.advance_nav_frames_for_test(3_000.0));
                let (puck, hud) = map.read_with(cx, |m, _| (m.nav_puck_screen(), m.nav_hud_rects()));
                let (px_, py) = puck.unwrap_or_else(|| panic!("{w}x{h} {mode:?}: the puck is on screen"));
                let r = gpui_maps::nav::chrome::PUCK_SIZE / 2.0 + MARGIN;
                let ahead = if mode == NavCameraMode::NorthUp2d { 0.0 } else { AHEAD };
                let keep_clear = gpui_maps::nav::chrome::Rect { x: px_ - r, y: py - r - ahead, w: 2.0 * r, h: 2.0 * r + ahead };
                assert!(px_ > 0.0 && px_ < w && py > 0.0 && py < h, "{w}x{h} {mode:?}: the puck ({px_}, {py}) is inside the map");
                let hud = hud.expect("an active trip");
                let mut rects: Vec<(String, gpui_maps::nav::chrome::Rect)> = Vec::new();
                for (name, rect) in [("the banner", hud.top), ("the strip", hud.strip), ("the tray", hud.tray)] {
                    rects.extend(rect.map(|r| (name.to_string(), r)));
                }
                for sel in ["nav-scrubber-toggle", "nav-view-mode-follow3d", "nav-view-mode-follow2d", "nav-view-mode-northup2d", "nav-view-mode-overview"] {
                    let b = cx.debug_bounds(sel).unwrap_or_else(|| panic!("{w}x{h}: {sel} did not draw"));
                    rects.push((sel.to_string(), gpui_maps::nav::chrome::Rect { x: f32::from(b.origin.x), y: f32::from(b.origin.y), w: f32::from(b.size.width), h: f32::from(b.size.height) }));
                }
                let chrome = map.read_with(cx, |m, _| m.nav_host_chrome().to_vec());
                rects.extend(chrome.into_iter().map(|r| ("the pill".to_string(), r)));
                for (name, rect) in rects {
                    let hit = rect.x < keep_clear.x + keep_clear.w && keep_clear.x < rect.x + rect.w && rect.y < keep_clear.y + keep_clear.h && keep_clear.y < rect.y + rect.h;
                    assert!(!hit, "{w}x{h} {mode:?}: {name} {rect:?} covers the puck at ({px_:.0}, {py:.0}) or the road ahead ({keep_clear:?})");
                }
            }
        }
    }

    /// A press on the toggle, a mode button or the pill between them stays
    /// with the scrubber: none reaches the map under it (a press that
    /// reaches the map grabs the camera and frees it).
    struct MapUnderHost(gpui::Entity<super::DriveScrubber>, std::rc::Rc<std::cell::Cell<u32>>);
    impl gpui::Render for MapUnderHost {
        fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            use gpui::InteractiveElement as _;
            let presses = self.1.clone();
            gpui::div()
                .relative()
                .size_full()
                .child(gpui::div().id("map-stand-in").absolute().top_0().left_0().size_full().on_mouse_down(gpui::MouseButton::Left, move |_, _, _| {
                    presses.set(presses.get() + 1);
                }))
                .child(self.0.clone())
        }
    }

    #[test]
    fn a_press_on_the_pill_never_reaches_the_map_under_it() {
        let mut app = TestAppContext::single();
        let map = simulated_drive_map(&mut app, 1280.0, 800.0);
        let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
        let presses = std::rc::Rc::new(std::cell::Cell::new(0));
        let (_host, cx) = app.add_window_view(|_, _| MapUnderHost(scrubber.clone(), presses.clone()));
        cx.run_until_parked();
        for (id, _) in MODE_BUTTONS {
            click(cx, id);
            assert_eq!(presses.get(), 0, "a press on {id} reached the map");
        }
        click(cx, "nav-scrubber-toggle");
        click(cx, "nav-scrubber-toggle");
        assert_eq!(presses.get(), 0, "a press on the timeline toggle reached the map");
        for (a, b) in [("nav-view-mode-follow3d", "nav-view-mode-follow2d"), ("nav-scrubber-toggle", "nav-view-mode-follow3d")] {
            let (a, b) = (cx.debug_bounds(a).unwrap(), cx.debug_bounds(b).unwrap());
            let gap = gpui::point((a.origin.x + a.size.width + b.origin.x) / 2.0, a.origin.y + a.size.height / 2.0);
            cx.simulate_mouse_down(gap, gpui::MouseButton::Left, gpui::Modifiers::default());
            cx.simulate_mouse_up(gap, gpui::MouseButton::Left, gpui::Modifiers::default());
            assert_eq!(presses.get(), 0, "nor does one on the pill between two buttons");
        }
        cx.simulate_mouse_down(gpui::point(gpui::px(10.0), gpui::px(10.0)), gpui::MouseButton::Left, gpui::Modifiers::default());
        assert_eq!(presses.get(), 1, "the bare map still gets its own presses");
    }

    /// On a window too narrow for the pill beside the tray it stacks above
    /// it (shrinking its buttons on a very narrow one), out of the HUD's
    /// own clearance: the pill hands its rect to the map, so the overview
    /// fits the route clear of it and the puck rides above it.
    #[test]
    fn the_pill_hands_its_rect_to_the_map_and_fits_the_window() {
        for (w, h) in [(1280.0_f32, 800.0_f32), (360.0_f32, 740.0_f32), (240.0_f32, 700.0_f32)] {
            let mut app = TestAppContext::single();
            let map = simulated_drive_map(&mut app, w, h);
            let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
            let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
            cx.simulate_resize(size(gpui::px(w), gpui::px(h)));
            cx.run_until_parked();
            let chrome = map.read_with(cx, |m, _| m.nav_host_chrome().to_vec());
            assert_eq!(chrome.len(), 1, "{w}x{h}: one pill");
            let pill = chrome[0];
            assert!(pill.x >= 0.0 && pill.x + pill.w <= w, "{w}x{h}: the pill fits the window: {pill:?}");
            for sel in ["nav-scrubber-toggle", "nav-view-mode-follow3d", "nav-view-mode-overview"] {
                let b = cx.debug_bounds(sel).expect("drawn");
                let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
                let (x1, y1) = (x0 + f32::from(b.size.width), y0 + f32::from(b.size.height));
                assert!(pill.contains(x0 + 0.5, y0 + 0.5) && pill.contains(x1 - 0.5, y1 - 0.5), "{w}x{h}: {sel} {b:?} inside the pill handed over {pill:?}");
            }
            if w < 400.0 {
                let tray = map.read_with(cx, |m, _| m.nav_hud_rects()).unwrap().tray.unwrap();
                assert!(pill.y + pill.h <= tray.y, "{w}x{h}: the pill stacks above the tray: {pill:?} vs {tray:?}");
            }
        }
    }

    /// A click outside the open timeline panel (the bare map) closes it --
    /// and leaves the toggle free to open it again right after.
    #[test]
    fn a_click_outside_the_panel_closes_it() {
        let mut app = TestAppContext::single();
        let map = simulated_drive_map(&mut app, 1280.0, 800.0);
        let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
        let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
        cx.run_until_parked();
        click(cx, "nav-scrubber-toggle");
        assert!(scrubber.read_with(cx, |s, _| s.open));
        cx.simulate_mouse_down(gpui::point(gpui::px(10.0), gpui::px(10.0)), gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.simulate_mouse_up(gpui::point(gpui::px(10.0), gpui::px(10.0)), gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(!scrubber.read_with(cx, |s, _| s.open), "a click well outside the panel closed it");
        click(cx, "nav-scrubber-toggle");
        assert!(scrubber.read_with(cx, |s, _| s.open), "the toggle still opens it right after");
    }

    /// The pill, its buttons and the open panel never overlap the HUD's
    /// banner, strip or tray, at a normal window and a small one, and never
    /// reach down into the brand mark's footprint.
    #[test]
    fn the_mode_buttons_and_panel_never_overlap_the_hud_or_the_brand_mark() {
        for (w, h) in [(1280.0_f32, 800.0_f32), (480.0_f32, 700.0_f32)] {
            let mut app = TestAppContext::single();
            let map = simulated_drive_map(&mut app, w, h);
            let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
            let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
            cx.simulate_resize(size(gpui::px(w), gpui::px(h)));
            cx.run_until_parked();
            let hud = map.read_with(cx, |m, _| m.nav_hud_rects()).expect("an active trip has HUD rects");
            let (_, mark_top, _, _) = gpui_maps::brand_mark::BrandMark::default().footprint(h);
            click(cx, "nav-scrubber-toggle");
            for sel in ["nav-view-mode-follow3d", "nav-view-mode-follow2d", "nav-view-mode-northup2d", "nav-view-mode-overview", "nav-scrubber-panel"] {
                let b = cx.debug_bounds(sel).unwrap_or_else(|| panic!("{w}x{h}: {sel} did not draw"));
                for (name, rect) in [("the banner", hud.top), ("the strip", hud.strip), ("the tray", hud.tray)] {
                    if let Some(rect) = rect {
                        assert!(!overlaps(b, rect), "{w}x{h}: {sel} {b:?} overlaps {name} {rect:?}");
                    }
                }
                let bottom = f32::from(b.origin.y) + f32::from(b.size.height);
                assert!(bottom <= mark_top, "{w}x{h}: {sel} reaches down to {bottom}, over the brand mark's top at {mark_top}");
            }
            let panel = cx.debug_bounds("nav-scrubber-panel").unwrap();
            for (sel, _) in MODE_BUTTONS {
                assert!(!bounds_overlap(panel, cx.debug_bounds(sel).unwrap()), "{w}x{h}: the panel covers {sel}");
            }
        }
    }

    /// Finds the quad `window` actually painted at `bounds`'s own origin
    /// (within a scaled pixel of rounding) -- not just that something
    /// laid out there (`debug_bounds` alone proves only that), but that a
    /// real quad with that origin is in the painted scene. `painted_quads`
    /// reports in `ScaledPixels` (`debug_bounds`'s own logical points
    /// times `scale_factor`), hence `scale`.
    fn quad_at(quads: &[gpui::Quad], bounds: gpui::Bounds<gpui::Pixels>, scale: f32) -> Option<&gpui::Quad> {
        let (x, y) = (f32::from(bounds.origin.x) * scale, f32::from(bounds.origin.y) * scale);
        quads.iter().find(|q| (q.bounds.origin.x.as_f32() - x).abs() < 1.0 && (q.bounds.origin.y.as_f32() - y).abs() < 1.0)
    }

    /// A TRIPWIRE: the timeline toggle's and the selected mode button's
    /// plates paint, opaque (`debug_bounds` proves layout, not paint).
    #[test]
    fn the_toggle_and_the_selected_mode_paint_an_opaque_round_plate() {
        let mut app = TestAppContext::single();
        let map = simulated_drive_map(&mut app, 1280.0, 800.0);
        let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
        let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
        cx.run_until_parked();
        let (quads, scale) = cx.update(|window, _| (window.painted_quads(), window.scale_factor()));
        for selector in ["nav-scrubber-toggle", "nav-view-mode-follow3d"] {
            let bounds = cx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} did not draw"));
            let plate = quad_at(&quads, bounds, scale).unwrap_or_else(|| panic!("{selector}: no quad painted at its own bounds {bounds:?} -- its plate never painted"));
            let solid = plate.background.as_solid().expect("a solid-colour plate, not a gradient");
            // The selected mode wears the banner's own green, 0.97 alpha.
            assert!(solid.a > 0.95, "{selector}: its plate is not opaque, alpha {}", solid.a);
        }
    }

    /// `a` is entirely within `b`, both `Bounds<Pixels>` in window points.
    fn fully_inside(a: gpui::Bounds<gpui::Pixels>, b: gpui::Bounds<gpui::Pixels>) -> bool {
        let (ax0, ay0) = (f32::from(a.origin.x), f32::from(a.origin.y));
        let (ax1, ay1) = (ax0 + f32::from(a.size.width), ay0 + f32::from(a.size.height));
        let (bx0, by0) = (f32::from(b.origin.x), f32::from(b.origin.y));
        let (bx1, by1) = (bx0 + f32::from(b.size.width), by0 + f32::from(b.size.height));
        ax0 >= bx0 && ay0 >= by0 && ax1 <= bx1 && ay1 <= by1
    }

    /// Every button's glyph sits fully inside its own plate with real
    /// padding on every side, at both window sizes (a glyph reaching a
    /// rounded plate's edge pokes past its visible curve).
    #[test]
    fn each_buttons_glyph_sits_inside_its_own_plate_with_padding() {
        for (w, h) in [(1280.0_f32, 800.0_f32), (480.0_f32, 700.0_f32), (240.0_f32, 700.0_f32)] {
            let mut app = TestAppContext::single();
            let map = simulated_drive_map(&mut app, w, h);
            let scrubber = app.update(|cx| super::drive_scrubber(&map, cx)).expect("the fixture route builds a scrubber");
            let (_host, cx) = app.add_window_view(|_, _| ScrubberHost(scrubber.clone()));
            cx.simulate_resize(size(gpui::px(w), gpui::px(h)));
            cx.run_until_parked();
            for (plate_sel, glyph_sel) in [
                ("nav-scrubber-toggle", "nav-scrubber-glyph"),
                ("nav-view-mode-follow3d", "nav-view-glyph-follow3d"),
                ("nav-view-mode-follow2d", "nav-view-glyph-follow2d"),
                ("nav-view-mode-northup2d", "nav-view-glyph-northup2d"),
                ("nav-view-mode-overview", "nav-view-glyph-overview"),
            ] {
                let plate = cx.debug_bounds(plate_sel).unwrap_or_else(|| panic!("{w}x{h}: {plate_sel} did not draw"));
                let glyph = cx.debug_bounds(glyph_sel).unwrap_or_else(|| panic!("{w}x{h}: {glyph_sel} did not draw"));
                assert!(fully_inside(glyph, plate), "{w}x{h}: {glyph_sel} {glyph:?} is not fully inside {plate_sel} {plate:?}");
                let pad_left = f32::from(glyph.origin.x) - f32::from(plate.origin.x);
                let pad_top = f32::from(glyph.origin.y) - f32::from(plate.origin.y);
                let pad_right = (f32::from(plate.origin.x) + f32::from(plate.size.width)) - (f32::from(glyph.origin.x) + f32::from(glyph.size.width));
                let pad_bottom = (f32::from(plate.origin.y) + f32::from(plate.size.height)) - (f32::from(glyph.origin.y) + f32::from(glyph.size.height));
                for (name, pad) in [("left", pad_left), ("top", pad_top), ("right", pad_right), ("bottom", pad_bottom)] {
                    assert!(pad > 1.0, "{w}x{h}: {glyph_sel} has only {pad}px of {name} padding inside {plate_sel}");
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Leaving the page pauses the drive.
    // -----------------------------------------------------------------

    struct Blank;
    impl gpui::Render for Blank {
        fn render(&mut self, _w: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    /// The Gallery's own `nav-drive` page, built as the Gallery builds it,
    /// on a runtime that can never reach the network.
    fn open_drive_page(cx: &mut gpui::VisualTestContext) -> (crate::kit::Built, gpui::Entity<MapView>) {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let handle = rt.handle().clone();
        rt.shutdown_background();
        let env = crate::kit::PanelEnv {
            host: crate::kit::Host::Gallery,
            runtime: handle,
            mapbox_token: None,
            viewport: (1280.0, 800.0),
            cancel: Default::default(),
            clickhouse: None,
        };
        let built = cx.update(|window, cx| super::build_nav_drive(&env, window, cx)).expect("the drive page builds keyless");
        let map = built.map.as_ref().and_then(|m| m.upgrade()).expect("the page has its map");
        (built, map)
    }

    /// The handles dropped: one effect cycle releases the entities (as
    /// the Gallery's own slot switch does, inside its update).
    fn flush(cx: &mut gpui::VisualTestContext) {
        cx.update(|_, _| {});
        cx.run_until_parked();
    }

    fn remaining(map: &gpui::Entity<MapView>, cx: &mut gpui::VisualTestContext) -> f64 {
        map.read_with(cx, |m, _| m.nav_state().expect("a trip").remaining_distance_m)
    }

    /// Whether the frames since the last call queued anything to say.
    fn spoke(map: &gpui::Entity<MapView>, cx: &mut gpui::VisualTestContext) -> bool {
        map.update(cx, |m, _| matches!(m.take_nav_voice_for_test(), Some(gpui_maps::nav::host::VoiceCommand::Speak(_))))
    }

    /// A page over the map (Settings, Exports, Home -- the Gallery calls
    /// `set_nav_suspended`): the car holds and nothing is said, however
    /// long; back, it drives on from there. Another card opened in its
    /// place drops the page: its place is kept, and the page opened again
    /// shows the car where it stopped and drives on. A drive the user
    /// stopped comes back stopped; an arrived one leaves nothing.
    #[test]
    fn leaving_the_drive_page_pauses_it_and_coming_back_drives_on_from_where_it_stopped() {
        let mut app = TestAppContext::single();
        let (_blank, cx) = app.add_window_view(|_, _| Blank);
        let (built, map) = open_drive_page(cx);

        // Driving: it moves and talks.
        let start = remaining(&map, cx);
        map.update(cx, |m, _| m.advance_nav_frames_for_test(20_000.0));
        let driving = remaining(&map, cx);
        assert!(start - driving > 50.0, "it drove: {start} -> {driving} m");
        assert!(spoke(&map, cx), "the drive's guidance was queued");

        // A page covers the map: a minute passes.
        map.update(cx, |m, _| m.set_nav_suspended(true));
        let _ = spoke(&map, cx);
        let held = remaining(&map, cx);
        let step = map.read_with(cx, |m, _| m.nav_state().unwrap().step_index);
        map.update(cx, |m, _| m.advance_nav_frames_for_test(60_000.0));
        assert_eq!(remaining(&map, cx), held, "the car held while the page was away");
        assert_eq!(map.read_with(cx, |m, _| m.nav_state().unwrap().step_index), step, "the maneuver held");
        assert!(!spoke(&map, cx), "nothing was queued to say while away");
        assert_eq!(map.read_with(cx, |m, _| m.nav_state().unwrap().sim.map(|s| s.paused)), Some(true), "it reports paused");

        // Back: it drives on from there.
        map.update(cx, |m, _| m.set_nav_suspended(false));
        map.update(cx, |m, _| m.advance_nav_frames_for_test(20_000.0));
        let back = remaining(&map, cx);
        assert!(held - back > 50.0, "back, it drove on: {held} -> {back} m");

        // Another card opened: the page and its map are dropped.
        let left = back;
        drop(map);
        drop(built);
        flush(cx);
        let kept = super::resume_slot().clone().expect("the drive's place was kept");
        assert!(!kept.paused && !kept.stopped);

        // The page opened again: the car where it stopped, driving on.
        let (built, map) = open_drive_page(cx);
        let resumed = remaining(&map, cx);
        assert!((resumed - left).abs() < 30.0, "the car is where it stopped: {resumed} vs {left} m");
        assert_eq!(map.read_with(cx, |m, _| m.nav_state().unwrap().sim.map(|s| s.paused)), Some(false), "and moving");
        map.update(cx, |m, _| m.advance_nav_frames_for_test(20_000.0));
        assert!(resumed - remaining(&map, cx) > 50.0, "it drives on");
        assert!(super::resume_slot().is_none(), "the place is used once");

        // An arrived drive leaves nothing: the page opened again starts afresh.
        map.update(cx, |m, _| m.advance_nav_frames_for_test(1_200_000.0));
        assert_eq!(map.read_with(cx, |m, _| m.nav_state().map(|s| s.phase)), Some(gpui_maps::nav::host::TripPhase::Arrived), "it arrived");
        drop(map);
        drop(built);
        flush(cx);
        assert!(super::resume_slot().is_none(), "nothing kept after arriving");
        let (built, map) = open_drive_page(cx);
        assert!((remaining(&map, cx) - start).abs() < 1.0, "a new drive from the start");

        // Stopped by the strip's Stop: it comes back stopped, and holds.
        map.update(cx, |m, _| m.advance_nav_frames_for_test(20_000.0));
        map.update(cx, |m, _| m.stop_nav_drive());
        let stopped_at = remaining(&map, cx);
        drop(map);
        drop(built);
        flush(cx);
        let (_built, map) = open_drive_page(cx);
        assert!(map.read_with(cx, |m, _| m.nav_state().unwrap().sim.is_some_and(|s| s.stopped)), "it comes back stopped");
        map.update(cx, |m, _| m.advance_nav_frames_for_test(20_000.0));
        let still = remaining(&map, cx);
        assert!((still - stopped_at).abs() < 30.0 && start - still > 50.0, "and holds where it stopped: {still} vs {stopped_at} m");
        assert!(!spoke(&map, cx), "a stopped drive says nothing");
    }
}

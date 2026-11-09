//! The standalone demo app's shell: winit's event loop, its window, and
//! `main`. A child of `app` so it drives `MapApp`'s private state directly;
//! compiled only with the `winit-shell` feature (on by default). An
//! embedding host uses `app::host` instead and links none of this.

use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::event::{TouchPhase as WinitPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use super::{demo_route_bytes, MapApp, SurfaceMetrics, TouchPhase};

/// The size, in PHYSICAL pixels, that the Metal drawable must be.
///
/// # `outer_size`, not `inner_size`, and this is not a preference
///
/// On iOS winit defines `inner_size()` as the **safe area** -- the window's
/// bounds minus `safeAreaInsets`, i.e. minus the status bar and the home
/// indicator (`platform_impl/ios/window.rs`, `safe_area_screen_space`).
/// `outer_size()` is the window's full bounds.
///
/// The layer wgpu draws into is neither of those by choice: it is a
/// `CAMetalLayer` that `raw-window-metal` adds as a sublayer of winit's
/// `UIView` with `frame = view.bounds`, and that view covers the WHOLE
/// window. So sizing the drawable from `inner_size()` hands Core Animation
/// a 402x778 pt image to display in an 402x874 pt layer, and it obligingly
/// scales it to fit. That is not an error, is not reported anywhere, and
/// looks like a map: it is simply 12.3% too tall, so that north-south
/// distances render 12.3% long against east-west ones.
///
/// (On an iPhone 16 Pro those numbers are 2622 vs 2334 physical pixels --
/// 62 pt of status bar plus 34 pt of home indicator, times 3. Every part of
/// that arithmetic is device-specific, which is exactly why the fix must be
/// to ASK, and never to subtract a constant.)
///
/// Touch coordinates agree with this choice rather than merely tolerating
/// it: winit takes them from `UITouch::locationInView(None)`, which is the
/// WINDOW's coordinate space -- full bounds, origin under the status bar's
/// top-left corner. Had the camera's viewport stayed on the safe area, every
/// unprojection would have been offset by the status bar's height as well.
fn drawable_size(window: &Window) -> winit::dpi::PhysicalSize<u32> {
    window.outer_size()
}

/// What `apply_resize` needs from the window: the drawable's physical size
/// (`drawable_size`), the scale, and the status bar / home indicator bands.
///
/// winit's iOS `inner_position` is the SAFE AREA's origin and
/// `outer_position` the window's (see `drawable_size`'s doc on the same
/// split for sizes), so their difference is the status bar and Dynamic
/// Island the compass must sit below. `inner_size` is the safe area, so
/// what it leaves of `outer_size` beyond the top inset is the home
/// indicator, which the chrome's bottom strip must sit above. Only the
/// insets are read from `inner_*`; the render stays on `outer_size`.
fn metrics_of(window: &Window) -> SurfaceMetrics {
    let physical = drawable_size(window);
    let scale = window.scale_factor();
    let top_inset_pt = match (window.inner_position(), window.outer_position()) {
        (Ok(inner), Ok(outer)) => ((inner.y - outer.y) as f64 / scale) as f32,
        _ => 0.0,
    };
    let bottom_inset_pt = {
        let (inner, outer) = (window.inner_size(), window.outer_size());
        ((f64::from(outer.height) - f64::from(inner.height)) / scale) as f32 - top_inset_pt
    }
    .max(0.0);
    SurfaceMetrics { width_px: physical.width, height_px: physical.height, scale, top_inset_pt, bottom_inset_pt }
}

fn phase_of(phase: WinitPhase) -> TouchPhase {
    match phase {
        WinitPhase::Started => TouchPhase::Started,
        WinitPhase::Moved => TouchPhase::Moved,
        WinitPhase::Ended => TouchPhase::Ended,
        WinitPhase::Cancelled => TouchPhase::Cancelled,
    }
}

/// Creates the window, its Metal surface and the renderer on it.
fn create_window_renderer(app: &MapApp, event_loop: &ActiveEventLoop) -> anyhow::Result<(Arc<Window>, super::Renderer)> {
    let window = Arc::new(event_loop.create_window(Window::default_attributes().with_title("gpui-maps"))?);
    // Metal only. `Backends::all()` would compile in nothing else here
    // anyway -- see `Cargo.toml`'s wgpu features -- but naming it makes the
    // failure mode legible if that ever changes.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = instance.create_surface(window.clone())?;
    // `-[UIScreen maximumFramesPerSecond]`, which winit surfaces as the
    // monitor's refresh rate: what the display link asks for.
    let screen_hz = window.current_monitor().and_then(|m| m.refresh_rate_millihertz()).map(|mhz| mhz as f32 / 1000.0);
    let mut renderer = app.create_renderer(&instance, surface, screen_hz)?;
    renderer.window = Some(window.clone());
    Ok((window, renderer))
}

impl ApplicationHandler for MapApp {
    /// iOS calls this once the app is active and there is a screen to draw
    /// on. Everything GPU-shaped is created here, not in `main`: on this
    /// platform there is no window before the app delegate says so.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }
        // Sized before the renderer exists: `create_renderer` configures the
        // surface from these.
        let window = match create_window_renderer(self, event_loop) {
            Ok((window, renderer)) => {
                self.surface_metrics = metrics_of(&window);
                self.install_renderer(renderer);
                window
            }
            Err(err) => {
                println!("[map] FAILED to create renderer: {err:#}");
                event_loop.exit();
                return;
            }
        };
        let hook_window = window.clone();
        self.redraw_hook = Some(Box::new(move || hook_window.request_redraw()));
        // The very first frame: nothing else is going to ask for it, since
        // no touch, resize or tile arrival has happened yet.
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // Coalesced into a flag rather than applied here: rotation and
            // the first layout pass both fire several of these, and
            // reconfiguring a surface plus reallocating a depth texture per
            // event thrashes badly. `apply_resize` re-reads the window when
            // it runs, so the final size is always the one applied.
            //
            // Still asks for a redraw right away, unconditionally: a resize
            // with nothing else going on must not sit there un-applied
            // until the next unrelated wake.
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(window) = self.renderer.as_ref().and_then(|r| r.window.as_ref()) {
                    self.surface_metrics = metrics_of(window);
                }
                self.resize_pending = true;
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {}
            WindowEvent::Touch(touch) => {
                // winit reports touches in PHYSICAL pixels; every gesture
                // and every camera call below is in logical points, the same
                // units the camera's viewport is in.
                if self.renderer.is_none() {
                    return;
                }
                let scale = self.surface_metrics.scale;
                self.on_touch(
                    phase_of(touch.phase),
                    touch.id,
                    touch.location.x / scale,
                    touch.location.y / scale,
                );
                // Every touch phase -- not just `Moved` -- can change what
                // the next frame must show (a tap starting a possible
                // double-tap zoom, a release throwing a fling, a third
                // finger switching the style), so every phase asks for one.
                // Redundant with the request `frame()` itself issues while
                // `pan_inertia` is live, never wrong to repeat.
                self.request_redraw();
            }
            _ => {}
        }
    }

    /// The wake `net::Fetcher` was given (see `main`, `Fetcher::set_wake`):
    /// a background thread finished a tile and there is now new data this
    /// `ControlFlow::Wait` loop would otherwise have no reason to notice.
    /// Draws nothing itself -- `frame()`'s own `collect_fetched` does that,
    /// on the next `about_to_wait` this unblocks.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        self.request_redraw();
    }

    /// The frame loop -- now genuinely on demand. `ControlFlow::Wait` means
    /// this run loop does not wake on its own at all; every wake is one of
    /// `request_redraw`'s callers below (a touch, a resize, `frame()` itself
    /// finding a fling still gliding or a bake backlog still draining, or
    /// `net::Fetcher`'s wake through `user_event`) having asked for exactly
    /// one more pass.
    ///
    /// `needs_redraw` is checked anyway, rather than trusting "we were woken
    /// so there must be a reason": `about_to_wait` fires after EVERY wake,
    /// including ones this app did not ask for (an accessibility
    /// notification, another app's window changing, in principle even the
    /// display link's own timer if `sync_activity_state` has not yet caught
    /// up with a gesture that just ended). None of those warrant a real GPU
    /// frame, and the flag is one bool compare against a `frame()` call
    /// that used to run unconditionally 60-120 times a second.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(match self.pump() {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }
}

/// The app's entry point. winit's iOS backend calls `UIApplicationMain`
/// inside `run_app` and never returns, so this IS `main`.
///
/// `Wait`, not `Poll`. This used to be `Poll`, on the reasoning that the map
/// always has work to do -- fetch results to collect, tiles to bake, a
/// glide to advance -- on frames where no event arrived, with the `Fifo`
/// surface's vsync pacing standing in for a redraw predicate. Measured on a
/// real ProMotion phone with the camera dead still, that reasoning cost
/// 33,857 frames and a hot phone: a `Fifo` present paces the RATE frames go
/// out at, it does not decide WHETHER one is owed, and nothing here was
/// asking that second question. `about_to_wait`'s `needs_redraw` check (and
/// every `request_redraw` call feeding it) is that question, asked
/// properly, and `Wait` is what makes asking it matter -- under `Poll` the
/// run loop wakes on its own regardless of what any flag says.
/// `name` from the bundle's own `Info.plist` `LSEnvironment` dictionary,
/// which `run-device.sh` writes when `GPUI_MAP_NAV` is set at deploy time.
/// A home-screen launch carries no arguments, and whether iOS copies
/// `LSEnvironment` into the process environment is not something to rest
/// the Simulate control on, so the app reads the dictionary itself.
fn bundle_env(name: &str) -> Option<String> {
    use objc2_foundation::{NSBundle, NSDictionary, NSString};
    let info = NSBundle::mainBundle().infoDictionary()?;
    let env = info.objectForKey(&NSString::from_str("LSEnvironment"))?.downcast::<NSDictionary>().ok()?;
    let value = env.objectForKey(&NSString::from_str(name))?.downcast::<NSString>().ok()?;
    Some(value.to_string())
}

pub fn main() -> anyhow::Result<()> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = MapApp::new()?;
    // The demo drive: `--nav-sim` / `GPUI_MAP_NAV=sim` starts it on the
    // first frame, `--nav-button` / `GPUI_MAP_NAV=button` offers the
    // engine-drawn Simulate control instead. Neither means no chrome at all.
    if let Some(request) = crate::nav::host::DemoRequest::from_args(&std::env::args().collect::<Vec<_>>())
        .or_else(|| crate::nav::host::DemoRequest::from_env(&|name| std::env::var(name).ok()))
        .or_else(|| crate::nav::host::DemoRequest::from_env(&bundle_env))
    {
        app.nav.set_demo_route(Arc::from(demo_route_bytes(&request.route)), request.options);
        app.nav.set_show_simulate(request.show_button);
        app.nav_autostart = request.autostart;
        println!(
            "[nav] demo route {:?}, autostart {}, button {}",
            request.route, request.autostart, request.show_button
        );
    }
    // The overlay demo toggle: `--overlay-demo`, `GPUI_MAP_OVERLAY=demo`, or
    // the same key in the bundle's `LSEnvironment`. Loads the engine's seeded
    // sample (`overlay::host::sample_blocks_geojson` + clustered points) --
    // the same data the browser's `?overlay=sample` draws.
    app.overlay_demo = std::env::args().any(|a| a == "--overlay-demo")
        || std::env::var("GPUI_MAP_OVERLAY").is_ok_and(|v| v == "demo")
        || bundle_env("GPUI_MAP_OVERLAY").is_some_and(|v| v == "demo");
    // Lets a tile landing on one of `net::Fetcher`'s worker threads wake
    // this now-`Wait` loop the instant it arrives, rather than waiting for
    // some unrelated touch or resize to bring the loop back around to
    // `collect_fetched`. `EventLoopProxy::send_event` is `Send` and cheap
    // (an OS wake, not a channel of its own -- the bytes still travel over
    // `Fetcher`'s existing `done` channel), so this costs nothing on the
    // hot fetch path.
    let proxy = event_loop.create_proxy();
    app.fetcher.set_wake(move || {
        let _ = proxy.send_event(());
    });
    // Population dots (Task 17): on when a PMTiles archive was copied into
    // the app's Documents folder (`run-device.sh`'s GPUI_MAP_DOTS_ARCHIVE);
    // `GPUI_MAP_DOTS=off` or `=<file>` in the environment or the bundle's
    // `LSEnvironment` turns them off or picks another archive.
    // MG-25's start views: `GPUI_MAP_AT=india|dhaka|us|nyc|nyc13|lon,lat,zoom`
    // (`shell_fetch::phone_start_view`), else Singapore as always.
    if let Some(at) = std::env::var("GPUI_MAP_AT").ok().or_else(|| bundle_env("GPUI_MAP_AT")) {
        match crate::overlay::host::shell_fetch::phone_start_view(&at) {
            Some((lon, lat, zoom)) => {
                app.camera_control.jump_to(crate::camera_control::CameraTarget {
                    center: Some((lon, lat)),
                    zoom: Some(zoom),
                    ..Default::default()
                });
                println!("[map] GPUI_MAP_AT={at}: starting at {lon},{lat} z{zoom}");
            }
            None => println!("[map] GPUI_MAP_AT={at} is not india, dhaka, us, nyc, nyc13 or lon,lat,zoom; starting at the default"),
        }
    }
    let dots_setting = std::env::var("GPUI_MAP_DOTS").ok().or_else(|| bundle_env("GPUI_MAP_DOTS"));
    if let Some((source, style)) = crate::dots::open_phone_source(dots_setting.as_deref()) {
        let proxy = event_loop.create_proxy();
        app.dot_worker = Some(crate::dots::DotWorker::new(move || {
            let _ = proxy.send_event(());
        }));
        app.dots.set_source(crate::dots::DOT_LAYER, source, style);
        app.dots_set_at = Some(Instant::now());
    }
    event_loop.run_app(&mut app)?;
    Ok(())
}

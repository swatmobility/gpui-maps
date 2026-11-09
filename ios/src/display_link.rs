//! The two UIKit/QuartzCore knobs winit does not expose and this map needs:
//! a `CADisplayLink` asking for 120 Hz, and the idle timer, both now scoped
//! to actual activity rather than held for the process's whole life -- see
//! [`FrameRateHint::set_paused`] and [`set_idle_timer_disabled`]. A map that
//! draws on demand and still asks Core Animation for 120 Hz and keeps the
//! screen from locking every second it is doing NOTHING is exactly the
//! battery/thermal defect this module used to have a hand in: an idle
//! `CADisplayLink` fires its (empty) callback every vsync regardless, which
//! is a real wake of the run loop even though it draws nothing, and an idle
//! timer disabled forever is a phone that never sleeps for a map nobody is
//! touching. `app.rs`'s `sync_activity_state` is what now calls both knobs,
//! on the same condition a fling or a finger on the glass.
//!
//! # Why this file exists at all
//!
//! `Info.plist`'s `CADisableMinimumFrameDurationOnPhone` lifts the 60 fps
//! CEILING that iOS puts on every iPhone app. It does not raise the rate.
//! Those are two different things, and the difference cost a measurement
//! round to establish:
//!
//! ```text
//! no key, no link              59.2 fps  p50 16.66 ms
//! key, no link                 61.7 fps  p50 16.65 ms   <- unchanged
//! key, this link               78.0 fps  p50  8.33 ms touch-to-present
//! ```
//!
//! with `[map] gpu:` reporting `screen max 120 Hz` throughout -- so the panel
//! was willing, Low Power Mode was off, and the accessibility frame-rate limit
//! was off. The cap was Core Animation's, and it was Core Animation's because
//! nothing in the process had ever said what rate it wanted.
//!
//! On a variable-refresh display the app is expected to declare its cadence.
//! Apple gives three ways to do it and this process has none of them:
//! `CADisplayLink.preferredFrameRateRange`; `MTKView.preferredFramesPerSecond`
//! (a display link underneath); and
//! `-[MTLCommandBuffer presentDrawable:afterMinimumDuration:]`. wgpu's Metal
//! backend presents with the plain `presentDrawable:`, winit's iOS backend
//! drives its loop from a `CFRunLoopTimer` rather than a display link, and
//! there is no `MTKView` anywhere -- so the whole process is silent on the
//! question and Core Animation answers it with the iPhone default of 60.
//!
//! Neither of the other two routes is reachable from here. The present call is
//! inside wgpu, and `src/` may not be touched. That leaves the display link,
//! which is a per-process request to Core Animation and needs no cooperation
//! from the renderer at all.
//!
//! # Why the callback does nothing
//!
//! Because the frame loop is already correct. `about_to_wait` -> `frame()` ->
//! `get_current_texture` blocks on `nextDrawable`, so the swapchain is the
//! pacer and it paces to the display's vsync -- whatever rate the display is
//! running at. This link's only job is to make that rate 120. Driving
//! `frame()` from the callback instead would mean taking the frame loop away
//! from winit's `ApplicationHandler`, which is a much larger change for a
//! cadence that is already right.
//!
//! The link is therefore a DECLARATION, not a driver, and the empty method
//! body below is the honest expression of that.

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{define_class, MainThreadOnly};
use objc2_foundation::{MainThreadMarker, NSRunLoop, NSRunLoopCommonModes};
use objc2_quartz_core::{CADisplayLink, CAFrameRateRange};

define_class!(
    // SAFETY:
    // - `NSObject` imposes no subclassing requirements.
    // - This type implements no `Drop`.
    // - `MainThreadOnly`, because the only thing that ever holds it is the
    //   display link, which is added to the main run loop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GpuiMapFrameRateHint"]
    struct FrameRateHintTarget;

    impl FrameRateHintTarget {
        /// Called once per display refresh, and deliberately empty -- see the
        /// module doc. `CADisplayLink` requires a target and a one-argument
        /// selector; it does not accept a block, and there is no stock class
        /// with a suitable no-op method to borrow, so the smallest honest
        /// thing is a class with one method that returns.
        #[unsafe(method(step:))]
        fn step(&self, _link: &CADisplayLink) {}
    }
);

/// A live `CADisplayLink`, kept alive by being held.
///
/// Dropping it invalidates the link and gives the refresh rate back to Core
/// Animation's default, so this is stored beside the renderer rather than
/// leaked: the request lasts exactly as long as there is something drawing.
pub struct FrameRateHint {
    link: Retained<CADisplayLink>,
    // Held only so the target outlives the link. `CADisplayLink` retains its
    // target, so this is belt and braces -- but a link whose target has been
    // deallocated is a crash, not a missed frame, and the cost of being sure
    // is one pointer.
    _target: Retained<FrameRateHintTarget>,
}

impl FrameRateHint {
    /// Asks Core Animation for `max` Hz, settling for no less than `min`.
    ///
    /// `preferred` is set equal to `max`: this app has no cheaper cadence it
    /// would rather run at, and a `preferred` below the maximum is read as
    /// "wake me at this rate", which is the opposite of what a map that is
    /// tracking a finger wants. `min` is NOT set equal to `max` either --
    /// pinning a range to a single value tells the system it may not drop the
    /// rate when the app cannot keep up, and a missed 120 Hz deadline then
    /// costs a full 8.3 ms of stall rather than a graceful step down to 80.
    ///
    /// Returns `None` on anything but a real ProMotion phone, where the
    /// request would be granted at 60 anyway and the link would be pure
    /// overhead.
    pub fn request(mtm: MainThreadMarker, min: f32, max: f32) -> Option<FrameRateHint> {
        if max <= 60.0 {
            return None;
        }
        let target = FrameRateHintTarget::alloc(mtm);
        let target: Retained<FrameRateHintTarget> = unsafe { objc2::msg_send![target, init] };
        // SAFETY: `step:` is defined above with exactly the signature
        // `CADisplayLink` calls it with, and `target` outlives the link.
        let link = unsafe {
            CADisplayLink::displayLinkWithTarget_selector(
                &target,
                objc2::sel!(step:),
            )
        };
        link.setPreferredFrameRateRange(CAFrameRateRange::new(min, max, max));
        // Common modes, not the default mode: the default mode is left the
        // moment a scroll or a system gesture starts a tracking run loop, and
        // a frame-rate request that lapses during exactly the interactions
        // this exists for would be worse than useless.
        //
        // SAFETY: called on the main thread, which `mtm` witnesses.
        unsafe {
            link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes);
        }
        Some(FrameRateHint { link, _target: target })
    }

    /// What the system actually granted, which need not be what was asked
    /// for -- Low Power Mode and Settings -> Accessibility -> Motion -> Limit
    /// Frame Rate both clamp it, silently.
    pub fn granted(&self) -> CAFrameRateRange {
        self.link.preferredFrameRateRange()
    }

    /// Pauses (or resumes) the link's callback without tearing it down and
    /// re-requesting the rate later.
    ///
    /// A paused `CADisplayLink` stops firing `step:` -- so it stops being a
    /// per-vsync wake of the run loop -- but keeps its
    /// `preferredFrameRateRange` request in place, ready the instant it is
    /// unpaused. That is the right shape for this app: the request itself
    /// (`FrameRateHint::request`) is made once, for the surface's whole
    /// life, but PAUSED whenever `app.rs`'s `sync_activity_state` finds no
    /// finger on the glass and no fling in flight, and unpaused the instant
    /// either becomes true again.
    pub fn set_paused(&self, paused: bool) {
        self.link.setPaused(paused);
    }
}

impl Drop for FrameRateHint {
    fn drop(&mut self) {
        self.link.invalidate();
    }
}

/// `-[UIApplication setIdleTimerDisabled:]`, so the phone does not auto-lock
/// mid-gesture or mid-fling -- and DOES auto-lock, exactly like every other
/// app, once the map has been sitting untouched for as long as the system
/// idle timeout.
///
/// Scoped rather than held for the process's whole life: a navigation app
/// that is actively guiding a turn earns keeping the screen on, but this map,
/// left open and untouched, does not, and "the phone gets hot and never
/// locks" was two symptoms of the same one cause, not two separate bugs --
/// see `app.rs`'s `sync_activity_state`, which calls this with `disabled`
/// tracking "a finger is on the glass or a fling is still gliding".
///
/// A phone that sleeps mid-gesture does not stop the process: it stops
/// handing out drawables, and the frame loop keeps running and keeps
/// logging. The signature is a present interval over a second long next to a
/// p50 that has quietly fallen back to 16.7 ms, and it is indistinguishable
/// from a real regression unless you happen to know the screen went dark.
/// Three measurement rounds were spent on exactly that before this call was
/// added -- which is exactly why it stays scoped to activity rather than
/// removed outright now that frames are on demand.
///
/// Done by selector rather than through `objc2-ui-kit`: this is one message
/// to one singleton, and the crate that would type it is a large one whose
/// only other use here would be nothing.
///
/// Returns whether the message was actually sent (`false` only when
/// `UIApplication` cannot be found, which does not happen on a real device);
/// the caller uses that to decide whether its own tracking of the current
/// state may be trusted.
pub fn set_idle_timer_disabled(_mtm: MainThreadMarker, disabled: bool) -> bool {
    use objc2::runtime::AnyClass;
    let Some(class) = AnyClass::get(c"UIApplication") else { return false };
    // SAFETY: `sharedApplication` is a main-thread-only class method
    // returning a `UIApplication` or nil, and `mtm` witnesses the thread.
    // `setIdleTimerDisabled:` takes one `BOOL`.
    unsafe {
        let app: *mut NSObject = objc2::msg_send![class, sharedApplication];
        if app.is_null() {
            return false;
        }
        let _: () = objc2::msg_send![app, setIdleTimerDisabled: disabled];
    }
    true
}

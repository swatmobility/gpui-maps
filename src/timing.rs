//! A monotonic clock that exists on every target this engine compiles for.
//!
//! # Why this module exists
//!
//! `std::time::Instant::now()` COMPILES for `wasm32-unknown-unknown` and
//! then panics at runtime with
//!
//! ```text
//! panicked at library/std/src/sys/time/unsupported.rs:13:9:
//! time not implemented on this platform
//! ```
//!
//! That combination -- compiles clean, aborts on the first call -- is the
//! worst shape a portability defect can take, and it is exactly the shape
//! that put a broken browser build on master. `./scripts/check-wasm.sh`
//! ran a full `cargo check --target wasm32-unknown-unknown` and returned
//! zero while `symbol_pass::LabelShapeCache::get_or_shape` was calling
//! `Instant::now()` on the first frame that had a font to shape with.
//!
//! The panic itself is survivable; what happened next is not. `web/`'s
//! release profile is `panic = "abort"` (and `wasm32-unknown-unknown` has
//! no unwinding to offer even if it were not), so the trap unwinds
//! NOTHING. The frame loop was inside `let mut a = app.borrow_mut()` when
//! it fired, so that `RefMut` guard was never dropped and the whole
//! application `RefCell` stayed mutably borrowed for the life of the page.
//! Every callback afterwards -- the resize observer, the window `resize`
//! listener, each tile fetch completing, the hidden-tab pump -- then
//! aborted in turn on `already borrowed`, one per event, forever. The
//! console filled with borrow panics pointing at five innocent call sites
//! and none at the one that caused them.
//!
//! So: one clock, one place, and a test that keeps it that way. See
//! `tests/wasm_portability.rs`.
//!
//! # What it is
//!
//! A drop-in for the slice of `std::time::Instant` this engine actually
//! uses -- `now`, `elapsed`, `duration_since` -- backed by
//! `std::time::Instant` everywhere except wasm, and by `performance.now()`
//! there.
//!
//! `performance.now()` and not `Date.now()`, which is what
//! [`crate::render::solar::now_unix_seconds`] reaches for through `js-sys`:
//! that function wants a WALL clock (a subsolar point is a function of the
//! date), this one wants a MONOTONIC one, and the two differ in resolution
//! as well as in kind. `Date.now()` is integer milliseconds, and the spans
//! measured through here are sub-millisecond -- `LabelShapeCache::shape_ms`
//! is tens of microseconds per label -- so a `Date.now()` backing would
//! report a flat zero for most of them and quietly retire an instrument
//! rather than fix it.
//!
//! `web-sys` is reached under `cfg(target_arch = "wasm32")` only, and the
//! only manifest that ever compiles this file for wasm is
//! `web/Cargo.toml`, which already carries `web-sys` with the `Window` and
//! `Performance` features. This is the same arrangement `solar.rs` makes
//! with `js-sys`, for the same reason: adding either to the shipped
//! crate's manifest would put `wasm-bindgen` into four applications'
//! lockfiles to satisfy a `cfg` arm their targets never take.

/// A measurement point on a monotonic clock.
///
/// The `std::time::Instant` subset this engine uses, portable to wasm. See
/// the module comment for why it is not `std::time::Instant` itself.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Instant(Inner);

#[cfg(not(target_arch = "wasm32"))]
type Inner = std::time::Instant;

/// Milliseconds since the page's time origin, as `performance.now()`
/// reports it. `f64` and not a `Duration` because that is the shape the
/// browser hands over, and converting once on read is cheaper than
/// converting on every `now()`.
#[cfg(target_arch = "wasm32")]
type Inner = f64;

/// Native only: a `std::time::Instant` already IS this clock's reading, so
/// code that has one (a native shell, a test) can hand it to an engine type
/// that stores this one.
#[cfg(not(target_arch = "wasm32"))]
impl From<std::time::Instant> for Instant {
    fn from(instant: std::time::Instant) -> Self {
        Self(instant)
    }
}

impl Instant {
    /// The current value of the clock.
    ///
    /// On wasm a missing `window` or `performance` (neither of which a
    /// document has ever lacked, but both of which are `Option` at this
    /// boundary) reads as zero rather than panicking. A zero clock makes
    /// every measured span zero, which is a visibly useless instrument --
    /// and a visibly useless instrument is the correct failure for
    /// something whose entire job is to be ignorable.
    #[must_use]
    pub fn now() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self(std::time::Instant::now())
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self(
                web_sys::window()
                    .and_then(|window| window.performance())
                    .map_or(0.0, |performance| performance.now()),
            )
        }
    }

    /// `self` moved `d` later: a test's scripted clock (frames at exact
    /// intervals), without reading `std`'s clock outside this file.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn after(&self, d: std::time::Duration) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self(self.0 + d)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self(self.0 + d.as_secs_f64() * 1e3)
        }
    }

    /// How long ago `self` was taken.
    #[must_use]
    pub fn elapsed(&self) -> std::time::Duration {
        Self::now().duration_since(*self)
    }

    /// The span from `earlier` to `self`, saturating at zero.
    ///
    /// Saturating rather than panicking on a reversed pair, which is what
    /// `std::time::Instant::duration_since` does as well: these are
    /// instrument reads, and an instrument that aborts the process is the
    /// defect this whole module exists to undo.
    #[must_use]
    pub fn duration_since(&self, earlier: Self) -> std::time::Duration {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.0.saturating_duration_since(earlier.0)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let millis = self.0 - earlier.0;
            // `performance.now()` is monotonic and finite by specification,
            // so neither guard should ever fire; both are here because a
            // NaN or a negative reaching `from_secs_f64` is a panic, and a
            // panic here is the thing being fixed.
            if millis.is_finite() && millis > 0.0 {
                std::time::Duration::from_secs_f64(millis / 1_000.0)
            } else {
                std::time::Duration::ZERO
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clock moves forward, and `elapsed` reports the movement.
    ///
    /// Deliberately weak on the upper bound -- a loaded CI box can stretch
    /// a 5 ms sleep a long way -- and strict on the lower, which is the
    /// half that distinguishes a working clock from a stub returning zero.
    #[test]
    fn the_clock_advances_and_elapsed_reports_it() {
        let start = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(4),
            "a 5 ms sleep measured as {elapsed:?}; the clock is not advancing"
        );
    }

    /// A reversed pair saturates rather than panicking. `std`'s own
    /// `duration_since` does the same; the wasm arm has to be told.
    #[test]
    fn a_reversed_span_is_zero_and_not_a_panic() {
        let first = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let second = Instant::now();
        assert_eq!(first.duration_since(second), std::time::Duration::ZERO);
        assert!(second.duration_since(first) > std::time::Duration::ZERO);
    }
}

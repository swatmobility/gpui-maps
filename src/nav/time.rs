//! The `SystemTime` Ferrostar's `UserLocation::timestamp` is, and the one
//! way this module tree makes one.
//!
//! Ferrostar compiles `std::time::SystemTime` natively and
//! `web_time::SystemTime` under its `wasm_js` feature (the browser build),
//! and the two are different types there. Naming the one that matches is
//! all this file does. It never reads a clock: `timestamp_at` turns a trip
//! time the caller already has into a timestamp, so a simulated drive is
//! the same drive every time it runs.

#[cfg(target_arch = "wasm32")]
pub use web_time::{SystemTime, UNIX_EPOCH};

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{SystemTime, UNIX_EPOCH};

/// `trip_ms` after the epoch. Negative or non-finite input is the epoch.
#[must_use]
pub fn timestamp_at(trip_ms: f64) -> SystemTime {
    let ms = if trip_ms.is_finite() { trip_ms.max(0.0) } else { 0.0 };
    UNIX_EPOCH + std::time::Duration::from_secs_f64(ms / 1000.0)
}

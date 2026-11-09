//! Turn-by-turn navigation: the route, the navigator, the turn text, the
//! simulated drive, the camera, the map layer and the chrome's layout --
//! everything navigation decides, in one place every shell shares.
//!
//! The rules every file here keeps (`tests/navigation_is_computed_in_nav.rs`
//! checks them):
//!
//! - no `gpui`, `web_sys`, `objc2`, `winit`, `wgpu` or `glyphon` type -- the
//!   browser (`web/src/lib.rs`) and the phone (`ios/src/lib.rs`) compile this
//!   directory by `#[path]`, and the GPU half is `render::nav_layer`;
//! - no clock: time arrives as `now_ms` and `format::WallClock`, never from
//!   `Instant::now`/`SystemTime::now` (which abort in a browser);
//! - no I/O and no printing: a shell fetches and logs, this only builds
//!   requests and reads bytes -- which is also what keeps the routing key out
//!   of every log line.
//!
//! Ferrostar (BSD-3) does the route model, snapping, step advance, deviation
//! detection and simulation; nothing here reimplements any of those.

pub mod chrome;
pub mod config;
pub mod demo;
pub mod follow;
pub mod format;
pub mod host;
pub mod icons;
pub mod instructions;
pub mod route_layer;
pub mod route_source;
pub mod session;
pub mod simulate;
pub mod time;
pub mod trip;
pub mod view_model;
pub mod voice;

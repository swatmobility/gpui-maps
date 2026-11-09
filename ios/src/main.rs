//! The bundle's `CFBundleExecutable`.
//!
//! There is no Swift host and no Xcode project. winit's iOS backend calls
//! `UIApplicationMain` from inside `run_app`, so this plain Rust binary IS
//! the application: `Info.plist` names it, `simctl install` accepts the
//! `.app` around it, and UIKit starts here.

fn main() {
    if let Err(err) = gpui_maps_ios::app::main() {
        // `simctl launch --console` is where this lands.
        eprintln!("[map] fatal: {err:#}");
        std::process::exit(1);
    }
}

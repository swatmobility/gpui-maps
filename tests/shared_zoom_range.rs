//! The zoom floor and ceiling are ONE shared pair, not three copies that can
//! quietly disagree.
//!
//! Before this gate, web and iOS each defined their own `const MIN_ZOOM: f64
//! = 1.0` / `const MAX_ZOOM: f64 = 19.0`, and native hardcoded `0.0, 20.0`
//! literals at its two `CameraController::new` call sites -- three numbers,
//! never checked against each other or against whether the globe camera
//! actually fits on screen at the low end. `1.0` was the one that stopped a
//! tilted globe from zooming out at all: see the bug report this fixed ("in
//! some positions I cannot zoom out from the Earth") and
//! `src/camera_control.rs`'s `MIN_ZOOM`/`MAX_ZOOM` doc comments for the
//! arithmetic that replaced the guess. Built like
//! `tests/one_cover_for_every_shell.rs`.

mod common;

use common::{code_only, read, repo_root};

const SHELLS: [&str; 3] = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];

#[test]
fn camera_control_defines_the_shared_zoom_constants_exactly_once() {
    let source = read(&repo_root().join("src/camera_control.rs"));
    let code = code_only(&source);
    assert_eq!(
        code.matches("pub const MIN_ZOOM").count(),
        1,
        "src/camera_control.rs must define `pub const MIN_ZOOM` exactly once"
    );
    assert_eq!(
        code.matches("pub const MAX_ZOOM").count(),
        1,
        "src/camera_control.rs must define `pub const MAX_ZOOM` exactly once"
    );
}

#[test]
fn no_shell_redefines_its_own_min_or_max_zoom() {
    for shell in SHELLS {
        let source = read(&repo_root().join(shell));
        let code = code_only(&source);
        // The colon anchors on the declaration form (`const MIN_ZOOM: f64 =
        // ...`), not just the substring `MIN_ZOOM`/`MAX_ZOOM` -- `map_view.rs`
        // has an unrelated `const MAX_ZOOM_LEVELS_PER_FRAME`, whose name
        // contains `const MAX_ZOOM` as a plain substring but is not a
        // redefinition of the shared constant.
        assert!(
            !code.contains("const MIN_ZOOM:") && !code.contains("const MAX_ZOOM:"),
            "{shell}: defines its own MIN_ZOOM/MAX_ZOOM constant instead of using \
             `crate::camera_control::{{MIN_ZOOM, MAX_ZOOM}}` -- that duplication is exactly how \
             web/iOS (1.0/19.0) and native (0.0/20.0) drifted apart before this gate existed"
        );
    }
}

#[test]
fn every_shell_s_camera_controller_is_built_with_the_shared_constants() {
    for shell in SHELLS {
        let source = read(&repo_root().join(shell));
        let code = code_only(&source);
        assert!(
            code.contains("CameraController::new"),
            "{shell}: expected to find a `CameraController::new` call to check"
        );
        assert!(
            code.contains("camera_control::MIN_ZOOM") || code.contains("MIN_ZOOM,") || code.contains("MIN_ZOOM)"),
            "{shell}: does not appear to reach for the shared MIN_ZOOM constant anywhere"
        );
        assert!(
            code.contains("camera_control::MAX_ZOOM") || code.contains("MAX_ZOOM,") || code.contains("MAX_ZOOM)"),
            "{shell}: does not appear to reach for the shared MAX_ZOOM constant anywhere"
        );
    }
}

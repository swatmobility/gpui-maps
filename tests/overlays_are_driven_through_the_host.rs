//! Overlays are orchestrated in `src/overlay/host.rs` and nowhere else.
//!
//! The `NavHost` rule (`tests/navigation_is_computed_in_nav.rs`) for caller
//! overlays: which tiles to bake and in what order, what landed, what goes on
//! the GPU and which stand-in draws while a tile is missing are decided in
//! one place, and every shell drives that place. Three shells that each
//! scheduled their own bakes would drift the way three shells once drifted
//! on the camera. Where code lives, not what it does, so a text scan is the
//! instrument.
//!
//! 1. `src/overlay/host.rs` names no platform (`gpui`, `web_sys`, `objc2`,
//!    `winit`), no executor (`tokio`, a thread spawn, `cx.spawn`), no I/O and
//!    no std clock. The browser and the phone compile it by `#[path]`; a
//!    browser aborts on a std clock read and cannot start a thread. (It does
//!    name `wgpu`: its GPU half holds the shell's buffers.)
//! 2. No file physically inside `web/src/` or `ios/src/` reaches an
//!    `overlay::` path other than `overlay::host` -- not the store, not the
//!    tile index, not the bakers. The data types a shell names
//!    (`FeaturePaint`, `ClusterOptions`, ...) are re-exported by the host.
//! 3. Every wired shell visibly drives the host, and native's `MapView`
//!    holds the shared `OverlayHost`/`OverlayGpu` rather than a copy of the
//!    types they replaced, and no longer calls the decisions they own.

mod common;

use common::{code_only, read, repo_root, rs_files};
use std::path::PathBuf;

const HOST: &str = "src/overlay/host.rs";
/// The dot-density module: shell-neutral under the same rules (the layer
/// is re-exported by the host, and the browser and the phone compile every
/// file here) -- except the two files that hold its I/O: `fetch_native`
/// (not wasm32) and `clickhouse` (its `connect` is native-only; the rest
/// -- the SQL, the RowBinary decode and the browser's proxy source -- is
/// sans-IO and every shell compiles it, Task 16).
const DOTGRID_DIR: &str = "src/overlay/dotgrid";
const DOTGRID_IO: [&str; 2] = ["fetch_native.rs", "clickhouse.rs"];

/// `src/overlay/dotgrid/*.rs` but its I/O files, sorted.
fn dotgrid_pure_files() -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(repo_root().join(DOTGRID_DIR))
        .expect("src/overlay/dotgrid exists")
        .map(|entry| entry.expect("a directory entry").file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs") && !DOTGRID_IO.contains(&name.as_str()))
        .map(|name| format!("{DOTGRID_DIR}/{name}"))
        .collect();
    files.sort();
    files
}

const HOST_BANS: [&str; 14] = [
    "gpui",
    "web_sys",
    "objc2",
    "winit",
    "tokio",
    "thread::spawn",
    "cx.spawn",
    "spawn_blocking",
    "std::time::Instant",
    "SystemTime::now",
    "std::fs",
    "reqwest",
    "ureq",
    "std::env",
];

/// `overlay::` paths a shell may name.
const ALLOWED_OVERLAY_PATHS: [&str; 1] = ["host"];

/// Shells wired onto the host, and what each must visibly call.
const WIRED: [(&str, &[&str]); 2] = [
    // The browser splits the draws: sources lifted over the buildings
    // (`setMarkers`' dots) go in a second list (`push_draws_split`).
    ("web/src/app.rs", &["ShellOverlays", "overlays.update(", "overlays.prepare(", "overlays.push_draws_split("]),
    ("ios/src/app.rs", &["ShellOverlays", "overlays.update(", "overlays.prepare(", "overlays.push_draws("]),
];

/// What `MapView` must use from the host, and the host-owned types and
/// decisions its production code must no longer define or call itself.
const NATIVE: &str = "src/map_view.rs";
const NATIVE_MUST_USE: [&str; 6] = [
    "OverlayHost::new(",
    "OverlayGpu::default()",
    ".request_tiles(",
    ".next_bake()",
    ".land_bake(",
    ".prepare_tiles(",
];
const NATIVE_MUST_NOT: [&str; 8] = [
    "struct OverlayTileSet",
    "struct OverlayBakeQueue",
    "struct OverlayTileGpuSet",
    "struct LiveState",
    "fn overlay_tile_picks",
    "fn stand_in_choice",
    "bake_overlay_tile_compact",
    "wanted_overlay_tiles(",
];

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True if `code` names an `overlay::` path whose next segment is not in
/// `ALLOWED_OVERLAY_PATHS` -- a use line, an alias, a qualified call.
fn has_banned_overlay_path(code: &str) -> bool {
    let bytes = code.as_bytes();
    let mut start = 0usize;
    while let Some(pos) = code.get(start..).and_then(|rest| rest.find("overlay::")) {
        let at = start + pos;
        if at == 0 || !is_ident_byte(bytes[at - 1]) {
            let after = &code[at + "overlay::".len()..];
            let allowed = ALLOWED_OVERLAY_PATHS.iter().any(|prefix| {
                after.strip_prefix(prefix).is_some_and(|rest| rest.as_bytes().first().is_none_or(|&b| !is_ident_byte(b)))
            });
            if !allowed {
                return true;
            }
        }
        start = at + 1;
    }
    false
}

fn lines_where(source: &str, bad: impl Fn(&str) -> bool) -> Vec<String> {
    let code = code_only(source);
    code.lines()
        .zip(source.lines())
        .enumerate()
        .filter(|(_, (code, _))| bad(code))
        .map(|(number, (_, original))| format!("{}: {}", number + 1, original.trim()))
        .collect()
}

/// `src/map_view.rs` up to its test module: the production half.
fn native_production_code() -> String {
    let source = read(&repo_root().join(NATIVE));
    let cut = source.find("#[cfg(test)]\nmod tests {").expect("map_view.rs has its test module");
    code_only(&source[..cut])
}

#[test]
fn the_scanners_find_what_they_are_for() {
    assert!(has_banned_overlay_path("let s = crate::overlay::OverlayStore::new();"));
    assert!(has_banned_overlay_path("use crate::overlay::tile_plan as tp;"));
    assert!(has_banned_overlay_path("crate::overlay::hostile::x();"), "a prefix of `host` is not `host`");
    assert!(!has_banned_overlay_path("let s = crate::overlay::host::ShellOverlays::new();"));
    assert!(!has_banned_overlay_path("my_overlay::thing();"), "only a whole-word `overlay::`");
    let bans = |code: &str| HOST_BANS.iter().any(|b| code.contains(b));
    assert_eq!(lines_where("// tokio in a comment\nlet x = tokio::spawn(f);\n", bans), ["2: let x = tokio::spawn(f);"]);
}

#[test]
fn the_host_names_no_platform_no_executor_no_io_and_no_std_clock() {
    let dotgrid = dotgrid_pure_files();
    assert!(dotgrid.iter().any(|f| f.ends_with("/layer.rs")) && dotgrid.len() >= 10, "the dotgrid scan lost its files: {dotgrid:?}");
    for file in std::iter::once(HOST.to_string()).chain(dotgrid) {
        let offenders = lines_where(&read(&repo_root().join(&file)), |code| HOST_BANS.iter().any(|b| code.contains(b)));
        assert!(
            offenders.is_empty(),
            "{file} must stay platform-, executor-, I/O- and std-clock-free -- the browser compiles it:\n  {}",
            offenders.join("\n  ")
        );
    }
}

#[test]
fn no_shell_file_reaches_past_the_host_into_the_overlay_machinery() {
    let root = repo_root();
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in ["web/src", "ios/src"] {
        let path = root.join(dir);
        assert!(path.is_dir(), "{dir} is missing -- the scan would check nothing");
        rs_files(&path, &mut files);
    }
    let mut offenders = Vec::new();
    for file in files {
        for line in lines_where(&read(&file), has_banned_overlay_path) {
            offenders.push(format!("{}:{line}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "a shell reaches past `overlay::host` (see ALLOWED_OVERLAY_PATHS):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_browser_and_the_phone_compile_the_one_overlay_module() {
    for lib in ["web/src/lib.rs", "ios/src/lib.rs"] {
        let text = read(&repo_root().join(lib));
        assert!(
            text.contains("#[path = \"../../src/overlay/mod.rs\"]"),
            "{lib} does not include src/overlay/mod.rs whole; the shell would draw no overlays, or its own"
        );
    }
}

#[test]
fn every_wired_shell_drives_overlays_through_the_host() {
    for (shell, needles) in WIRED {
        let code = code_only(&read(&repo_root().join(shell)));
        for needle in needles {
            assert!(code.contains(needle), "{shell} is wired but never calls `{needle}`");
        }
    }
}

/// The shells that draw population dots (Task 16 the browser, Task 17 the
/// phone), each driving the engine's `DotLayers` whole: planned, jobs
/// handed out and landed, synced to the GPU and drawn -- and at its own
/// platform's style, not a budget of its own: `shell_fetch::shell_dot_style`
/// for the platform, which is `DotStyle::for_platform` or, for a race
/// archive, `DotStyle::race` on `race_budget_for` (race plan, MG-R15).
const DOTS_WIRED: [(&[&str], &[&str]); 2] = [
    (
        &["web/src/app.rs"],
        &[
            "dots.frame(",
            "dots.next_job()",
            "dots.land(",
            "dots_gpu.sync(",
            "dots_gpu.push_draws(",
            "shell_fetch::shell_dot_style(",
            "Platform::Web",
        ],
    ),
    (
        &["ios/src/app.rs", "ios/src/dots.rs"],
        &[
            "dots.frame(",
            "dots.next_job()",
            "dots.land(",
            "dots_gpu.sync(",
            "dots_gpu.push_draws(",
            "shell_fetch::shell_dot_style(Platform::Ios,",
        ],
    ),
];

#[test]
fn every_dot_shell_drives_the_whole_dot_layer() {
    for (files, needles) in DOTS_WIRED {
        let code: String = files.iter().map(|f| code_only(&read(&repo_root().join(f)))).collect::<Vec<_>>().join("\n");
        for needle in needles {
            assert!(code.contains(needle), "{files:?} draw dots but never call `{needle}`");
        }
    }
}

#[test]
fn map_view_drives_the_shared_host_and_keeps_no_copy_of_it() {
    let code = native_production_code();
    for needle in NATIVE_MUST_USE {
        assert!(code.contains(needle), "{NATIVE} no longer uses `{needle}` from overlay::host");
    }
    for needle in NATIVE_MUST_NOT {
        assert!(
            !code.contains(needle),
            "{NATIVE} defines or calls `{needle}` itself; it belongs to overlay::host, which every shell shares"
        );
    }
}

/// Definitions only `overlay::dotgrid::mask_step` may hold: when the dot
/// mask is re-rendered is decided once, for every shell (dasymetric plan
/// Task 10, DP-7) -- a copy in `map_view` would drift from the one the
/// browser and the phone drive.
const MASK_STEP_ONLY: [&str; 6] = [
    "fn step_dot_mask",
    "fn frame_mask_key",
    "fn cover_is_complete",
    "struct MaskRekey",
    "struct DrawnBasemapTile",
    "fn mask_ring_for_frame(\n    layers",
];
const MASK_STEP: &str = "src/overlay/dotgrid/mask_step.rs";

#[test]
fn the_dot_mask_step_is_shared_and_map_view_only_drives_it() {
    let step = code_only(&read(&repo_root().join(MASK_STEP)));
    for needle in MASK_STEP_ONLY {
        assert!(step.contains(needle), "{MASK_STEP} lost `{needle}`");
    }
    let mut files = Vec::new();
    rs_files(&repo_root().join("src"), &mut files);
    for file in files.iter().filter(|f| !f.ends_with(MASK_STEP)) {
        let code = code_only(&read(file));
        for needle in MASK_STEP_ONLY {
            assert!(
                !code.contains(needle),
                "{} defines `{needle}` itself; it belongs to {MASK_STEP}, which every shell shares",
                file.display()
            );
        }
    }
    let native = native_production_code();
    for needle in ["overlay::host::step_dot_mask(", "overlay::host::MaskRekey", "overlay::host::mask_ring_for_frame("] {
        let in_dots = code_only(&read(&repo_root().join("src/map_view/dots.rs")));
        assert!(native.contains(needle) || in_dots.contains(needle), "map_view no longer drives `{needle}`");
    }
}

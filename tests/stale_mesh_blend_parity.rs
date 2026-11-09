//! Every shell blends a tile's baked paint pair against THAT TILE's bake
//! zoom, never against `camera.zoom.fract()`.
//!
//! A mesh carries each zoom-dependent paint value as a pair, `(v@b, v@b+1)`
//! for its bake bucket `b`, and the shader mixes them by the model uniform's
//! `blend_t`. When the camera crosses an integer zoom, every resident mesh
//! is a bucket behind until it is re-baked -- which on the browser's main
//! thread takes tens of frames at pitch 60. Feeding such a STALE mesh
//! `fract()` wraps: at z16.03 a mesh baked at 15 got `t = 0.03`, i.e. its
//! z15 widths and colours, for every frame until its re-bake, and then
//! jumped to z16's. That was the browser's "twitchy on some zoom changes":
//! road casings and building fills snapping back one bucket and forward
//! again, tile by tile. Native had already fixed this with a clamp
//! (`render::tile_blend_t_for_baked_zoom`); the browser and iOS still wrapped.
//!
//! This fails the build if a shell's per-tile uniform write goes back to
//! `fract()`. Built like `tests/one_cover_for_every_shell.rs`.

mod common;

use common::{code_only, read, repo_root};

const SHELLS: [&str; 3] = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];
const SHARED: &str = "tile_blend_t_for_baked_zoom(";

/// The body of the per-tile vector uniform loop -- `for tile in &plan.draws`,
/// the loop that rewrites every drawn tile's model uniform -- in the web and
/// iOS shells, `window` lines of it.
fn draws_uniform_loop(code: &str, window: usize) -> Option<String> {
    let lines: Vec<&str> = code.lines().collect();
    let start = lines.iter().position(|l| l.trim_start().starts_with("for tile in &plan.draws {"))?;
    Some(lines[start..(start + window).min(lines.len())].join("\n"))
}

#[test]
fn every_shell_blends_a_tile_against_its_own_bake_zoom() {
    let root = repo_root();
    let mut missing = Vec::new();
    for shell in SHELLS {
        let code = code_only(&read(&root.join(shell)));
        if !code.contains(SHARED) {
            missing.push(shell);
        }
    }
    assert!(missing.is_empty(), "these shells never call the shared `{SHARED}`: {missing:?}");
}

#[test]
fn the_browser_and_ios_tile_uniform_loops_do_not_wrap_a_stale_pair() {
    let root = repo_root();
    for shell in ["web/src/app.rs", "ios/src/app.rs"] {
        let code = code_only(&read(&root.join(shell)));
        let body = draws_uniform_loop(&code, 25).unwrap_or_else(|| panic!("{shell}: no `for tile in &plan.draws` loop"));
        assert!(body.contains("write_model_uniform"), "{shell}: the loop found is not the uniform write:\n{body}");
        assert!(body.contains(SHARED), "{shell}: the per-tile uniform does not use `{SHARED}`:\n{body}");
        assert!(!body.contains("zoom.fract()"), "{shell}: the per-tile uniform still wraps with `fract()`:\n{body}");
    }
}

#[test]
fn the_loop_finder_finds_what_it_is_for() {
    let planted = "fn draw() {\n    for tile in &plan.draws {\n        pipeline.write_model_uniform(a, zoom.fract() as f32);\n    }\n}\n";
    let body = draws_uniform_loop(planted, 25).expect("finds the loop");
    assert!(body.contains("zoom.fract()"));
    assert!(draws_uniform_loop("fn nothing() {}", 25).is_none());
}

//! The browser had **no raster pipeline at all** until this task: satellite
//! or relief imagery set through a style's `"type": "raster"` source could
//! never appear in `web/src/app.rs`, no matter how the style was written,
//! because nothing there ever asked whether a frame's imagery should draw,
//! at what opacity, or at what cover zoom. `src/map_view.rs` had all three
//! answers, written out as its own private methods.
//!
//! `render::raster_plan::raster_plan` is now the one function every shell
//! that draws imagery calls for that decision -- native's
//! `MapView::raster_opacity_for_frame`/`MapView::raster_tiles_for_frame`
//! delegate to it, and so does the browser's new `MapApp::raster_plan`/
//! `MapApp::raster_cover`. This test is the reason a shell cannot quietly go
//! back to walking `StyleLayer::Raster` itself, or to reinventing
//! `raster_cover_zoom`'s tile-size/scale-factor correction, the same way
//! `space_plan_parity.rs` and `label_plan_parity.rs` are for the space pass
//! and the label pass.
//!
//! iOS used to draw no imagery either -- `ios/src/render/mod.rs` included
//! `raster_mesh`/`raster_pipeline` (they compiled there unconditionally,
//! since `gpu.rs`'s `RasterDraws` names them) but built no `RasterPipeline`
//! -- so it was excluded from this file's shell list rather than held to a
//! standard for a pass it had never drawn. `ios/src/app.rs` now calls
//! `raster_plan::raster_plan` for the decision and builds a real
//! `RasterPipeline` (`GPUI_MAP_IMAGERY`/`_TILE_SIZE`/`_MAX_ZOOM` env vars,
//! this crate's `?imagery=` equivalent -- see `raster_config`'s own doc
//! comment), so it is held to the same standard here as native and the
//! browser.
//!
//! Structured like `tests/space_plan_parity.rs` and `tests/label_plan_parity.rs`:
//! read the repo root off `CARGO_MANIFEST_DIR`, and guard against a scan that
//! silently matches nothing passing as success.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Every shell that draws imagery. `ios/src/app.rs` joined this list once
/// it built a real `RasterPipeline` -- see this file's own module doc.
const RASTER_DRAWING_SHELLS: [&str; 3] = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];

/// `src/map_view.rs` carries a `#[cfg(test)] mod tests {..}` tail that
/// constructs a literal `style::StyleLayer::Raster { .. }` as a test
/// fixture -- a perfectly legitimate use that has nothing to do with a
/// shell deriving its OWN opacity decision. Excluding it is what makes the
/// "shell does not walk `StyleLayer::Raster` itself" assertion below mean
/// something, the same boundary `space_plan_parity.rs`'s `production_code`
/// draws for the identical reason. A no-op on `web/src/app.rs`, which has
/// no such tail.
fn production_code(source: &str) -> &str {
    match source.find("\nmod tests {") {
        Some(at) => &source[..at],
        None => source,
    }
}

fn assert_not_suspiciously_short(shell: &str, src: &str) {
    assert!(
        src.len() > 2_000,
        "{shell} read as only {} bytes -- suspiciously short for a shell that builds a frame. \
         This test's scan may be reading the wrong file.",
        src.len(),
    );
}

/// Every imagery-drawing shell must build this frame's raster decision
/// through the one shared function rather than re-deriving the
/// style-layer-walk / cover-zoom sequence itself.
///
/// A shell that stops calling `raster_plan` can reintroduce the exact
/// defect this module exists to remove: a partial or diverging copy of the
/// opacity/cover-zoom decision, with nothing to notice until someone runs
/// the browser build and finds imagery missing, wrong at some zooms, or
/// magnified past the point `raster_cover_zoom`'s `round` exists to avoid.
#[test]
fn every_raster_drawing_shell_calls_the_shared_raster_plan() {
    for shell in RASTER_DRAWING_SHELLS {
        let src = read(&repo_root().join(shell));
        assert_not_suspiciously_short(shell, &src);

        assert!(
            src.contains("raster_plan::raster_plan("),
            "{shell} never calls `render::raster_plan::raster_plan` -- it must decide whether \
             this frame's imagery draws, at what opacity and at what cover zoom, through the \
             one shared function rather than re-deriving that decision itself. That \
             re-derivation is exactly how the browser build had no raster pipeline at all -- \
             see `src/render/raster_plan.rs`'s module doc.",
        );
    }
}

/// The specific formula this module exists to stop a shell from
/// reinventing: `raster_cover_zoom`'s tile-size/scale-factor correction and
/// its **`round`, not `floor`** rounding (a deliberate divergence from the
/// vector cover's `floor` -- see `render::raster_plan`'s own doc comment
/// and the raster task's hard constraints for why the two must not be
/// unified). A shell that defines its own copy can silently pick `floor`
/// again, which is exactly the two-independent-magnifications defect
/// `raster_cover_zoom` was written to fix in the first place.
#[test]
fn no_shell_defines_its_own_raster_cover_zoom_formula() {
    for shell in RASTER_DRAWING_SHELLS {
        let src = read(&repo_root().join(shell));
        assert_not_suspiciously_short(shell, &src);

        let banned = "fn raster_cover_zoom(";
        let direct = src.matches(banned).count();
        assert_eq!(
            direct, 0,
            "{shell} defines its own `{banned}` {direct} time(s); it must import \
             `render::raster_plan::raster_cover_zoom` (or call `raster_plan::raster_plan`, which \
             calls it internally) instead of carrying a second copy of the tile-size/scale-factor \
             correction. Two copies is exactly how the round-vs-floor asymmetry this function \
             exists to preserve could quietly drift apart between shells.",
        );
    }
}

/// The other half of the same defect: a shell that walks `StyleLayer::Raster`
/// itself to resolve an opacity, rather than reading `RasterPlan::opacity`.
/// `production_code` excludes `src/map_view.rs`'s test fixture, which
/// constructs the variant as data and decides nothing from it.
#[test]
fn no_shell_walks_style_layer_raster_to_resolve_opacity_itself() {
    for shell in RASTER_DRAWING_SHELLS {
        let full_src = read(&repo_root().join(shell));
        assert_not_suspiciously_short(shell, &full_src);
        let src = production_code(&full_src);

        let banned = "StyleLayer::Raster {";
        let direct = src.matches(banned).count();
        assert_eq!(
            direct, 0,
            "{shell} destructures `{banned}` {direct} time(s) in its production code -- it must \
             read `RasterPlan::opacity` (from `raster_plan::raster_plan`) instead of walking the \
             style's raster layers and resolving `raster-opacity` itself. That walk is exactly \
             `raster_opacity_for_frame`'s old body, and duplicating it is how two shells could \
             disagree about the zoom range or the style-spec default for a layerless source.",
        );
    }
}

/// The browser's specific historical gap (and iOS's, until this task),
/// stated as a positive assertion rather than only the absences above:
/// a shell must actually fetch, decode and draw imagery, not merely link
/// against types that are never exercised. A shell that called
/// `raster_plan` correctly but built no `RasterPipeline` and issued no
/// raster draw call would satisfy every test above while still drawing
/// nothing -- a different failure from the ones this file otherwise
/// catches, but a silent one nonetheless.
#[test]
fn every_non_native_shell_builds_a_raster_pipeline_and_draws_with_it() {
    for shell in ["web/src/app.rs", "ios/src/app.rs"] {
        let src = read(&repo_root().join(shell));
        assert_not_suspiciously_short(shell, &src);

        for needle in ["RasterPipeline::new(", "render_with_space_and_raster("] {
            assert!(
                src.contains(needle),
                "{shell} never calls `{needle}` -- deciding to draw imagery is not the same \
                 as drawing it. See `src/render/raster_pipeline.rs` and \
                 `render::gpu::TilePipeline::render_with_space_and_raster`.",
            );
        }
    }
}

/// Proves `production_code`'s boundary is doing real work on THIS file, the
/// same proof `space_plan_parity.rs` runs for its own use of the same
/// helper -- excluding too much would make
/// `no_shell_walks_style_layer_raster_to_resolve_opacity_itself` pass by
/// scanning nothing, and excluding too little would make it fail on the
/// test module's own fixture rather than on a real defect.
#[test]
fn production_code_excludes_map_views_test_module_but_not_its_raster_plan_call() {
    let src = read(&repo_root().join("src/map_view.rs"));
    let production = production_code(&src);

    assert!(
        production.len() < src.len(),
        "production_code found no `mod tests {{` boundary in src/map_view.rs at all -- either \
         the file lost its test module or this scan stopped matching it",
    );
    assert!(
        production.contains("raster_plan::raster_plan("),
        "production_code cut off real code: `MapView::raster_opacity_for_frame`'s call to \
         `raster_plan::raster_plan` should be well before the test module",
    );
    assert!(
        !production.contains("StyleLayer::Raster {"),
        "production_code did not actually exclude the test module -- src/map_view.rs's test \
         fixture, which constructs a literal `style::StyleLayer::Raster {{ .. }}`, is still \
         inside the excluded region as far as this scan is concerned. If this now fails, either \
         the boundary string moved or the fixture did.",
    );
}

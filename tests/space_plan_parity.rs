//! The shells may differ in INPUT (gpui events, DOM events, UIKit) and in
//! the GPU handles they hold. They may not differ in what a frame contains.
//!
//! This test exists because they silently did, for months: the browser drew
//! no stars and iOS never has, each discovered separately -- see
//! `docs/superpowers/plans/2026-09-22-frame-core-step-1-space.md`. Tasks 1-4
//! of that plan made all three shells call one function,
//! `render::frame_plan::space_plan`. Nothing before this file stopped a
//! fourth shell, or a future edit to one of these three, from deciding for
//! itself again. A count is not a gate; this is.
//!
//! Structured to match `tests/ios_shell_stays_in_step.rs`, which guards the
//! same class of drift for the iOS module list: read the repo root off
//! `CARGO_MANIFEST_DIR` rather than hardcoding it, and never let a scan that
//! silently matches nothing pass as success.

use gpui_maps::render::atmosphere::Atmosphere;
use gpui_maps::render::frame_plan::space_plan;
use gpui_maps::render::space::Space;
use gpui_maps::style::SkyFrame;

mod common;

use common::{read, repo_root};

/// The fixture every shell must agree on: stars fully on, so the plan's
/// three booleans are exercised rather than trivially false throughout.
fn fixture() -> (Space, Atmosphere) {
    (Space { star_intensity: 1.0, ..Space::default() }, Atmosphere::default())
}

/// `space_plan` takes four plain values and returns four plain values. Two
/// calls with the same inputs must return the same output, at every
/// transition a frame can actually be at -- not just the two endpoints.
#[test]
fn space_plan_is_a_pure_function_of_its_inputs() {
    let (space, atmosphere) = fixture();
    for transition in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
        for above_horizon in [false, true] {
            let a = space_plan(None, &space, &atmosphere, &SkyFrame::default(), transition, above_horizon);
            let b = space_plan(None, &space, &atmosphere, &SkyFrame::default(), transition, above_horizon);
            assert_eq!(
                a, b,
                "space_plan returned different SpacePlan values for identical inputs \
                 at transition {transition} -- it must be a pure function, with \
                 nothing hidden in `Space`, `Atmosphere` or global state affecting \
                 the result"
            );
        }
    }
}

/// Every shell must read `plan.draws_stars` / `plan.draws_backdrop` rather
/// than calling `Space::draws_stars` or `space::draws_backdrop` itself. A
/// shell that calls the gate directly has re-introduced the exact defect
/// this plan step exists to remove: it can compute a DIFFERENT answer than
/// the other two shells, and nothing will notice until someone screenshots
/// the wrong platform.
#[test]
fn no_shell_calls_a_space_gate_directly() {
    let shells = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];

    for shell in shells {
        let path = repo_root().join(shell);
        let src = read(&path);

        // A scan of an empty or truncated read would pass every assertion
        // below while checking nothing -- the same failure mode
        // `ios_shell_stays_in_step.rs` guards against for its module list.
        assert!(
            src.len() > 2_000,
            "{shell} read as only {} bytes -- suspiciously short for a shell \
             that builds a frame. This test's scan may be reading the wrong \
             file, or the file may have lost its frame-building code; either \
             way, an assertion of zero direct calls below would be worthless.",
            src.len(),
        );

        let mut via_plan = 0usize;
        for banned in ["draws_stars(", "draws_backdrop("] {
            let direct = src.matches(banned).count();
            assert_eq!(
                direct, 0,
                "{shell} calls `{banned}` directly {direct} time(s); it must read \
                 `plan.{}` (the field `space_plan` returns) instead. A shell \
                 deciding this gate for itself is exactly how the browser lost \
                 its stars and iOS never drew any -- see \
                 docs/superpowers/plans/2026-09-22-frame-core-step-1-space.md.",
                banned.trim_end_matches('('),
            );

            let field = format!(".{}", banned.trim_end_matches('('));
            via_plan += src.matches(field.as_str()).count();
        }

        // The zero-direct-calls assertion above is trivially satisfied by a
        // shell that dropped the space pass entirely, which is not the same
        // thing as a shell that reads it from the plan. Requiring at least
        // one field read tells the two apart.
        assert!(
            via_plan > 0,
            "{shell} never reads `.draws_stars` or `.draws_backdrop` at all -- \
             it calls neither the gate directly nor the plan's fields, so it \
             may have dropped the space pass rather than adopted the plan. \
             That would satisfy the assertion above for the wrong reason.",
        );
    }
}

/// The SKY half of the same class of drift: every shell hands the plan its
/// style's `sky` at the frame's zoom, draws the sky pass on the plan's
/// `draws_sky`, and writes the scattering pass with the plan's
/// `atmosphere_intensity` (the style's `atmosphere-blend` folded in) --
/// never by deciding either for itself.
///
/// Before `render::sky` the three shells cleared above the limb to a lerp
/// toward the style background: the grey sky. A shell that skipped the sky
/// pass would bring that back as a black one, and nothing else would notice.
#[test]
fn every_shell_draws_the_sky_the_plan_decides() {
    let shells = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];
    for shell in shells {
        let src = read(&repo_root().join(shell));
        assert!(src.len() > 2_000, "{shell} read as only {} bytes", src.len());
        let src = production_code(&src);
        for banned in ["draws_sky(", "frame_atmosphere.draws("] {
            assert_eq!(
                src.matches(banned).count(),
                0,
                "{shell} calls `{banned}` directly; it must read the SpacePlan's field instead",
            );
        }
        // `sees_above_horizon(` because the sky is gated on GEOMETRY: a shell
        // that handed the plan a literal `false` would bring back the flat
        // background above the curved horizon at z6.5, tilted.
        for needed in [".sky.at_zoom(", ".draws_sky", ".atmosphere_intensity", "SkyPipeline::new(", "sky::sees_above_horizon("] {
            assert!(
                src.contains(needed),
                "{shell} never mentions `{needed}` -- it has not wired the style's sky through \
                 `render::frame_plan::space_plan` and `render::sky::SkyPipeline`, so its globe \
                 band shows no sky above the limb",
            );
        }
    }
}

/// The tile-cover half of the same class of drift.
///
/// `web/src/app.rs`'s `cover()`/`prefetch_cover()` and `ios/src/app.rs`'s
/// `cover_at()` used to call `crate::geo::visible_tiles` directly and
/// unconditionally -- a flat Mercator viewport bbox whose x range is clamped
/// to `[0, n-1]`. A flat bbox cannot cover a sphere showing a hemisphere, so
/// the browser (and iOS) rendered a globe with roughly half of it blank grey
/// -- Liberty's own `background-color: #f8f4f0` showing through wherever no
/// tile was ever requested. `src/map_view.rs` never had this bug: its
/// `MapView::tiles_for_zoom` already asked `render::globe::GlobeCover`
/// first. `render::cover_plan::tile_cover` is that same routing, pulled out
/// so every shell goes through it -- this test is what stops a shell from
/// quietly going back to `geo::visible_tiles` on its own.
///
/// Only `src/map_view.rs` carries a `#[cfg(test)] mod tests { .. }` tail
/// today, and it contains a DELIBERATE, unrelated direct call --
/// `tilt_clamp_alone_still_allows_a_visible_set_over_the_cap_at_high_zoom`
/// recomputes the flat cover by hand to check `MAX_VISIBLE_TILES` capping
/// against an independent expectation, not against the production path.
/// `production_code` below excludes that tail (and any test code the other
/// two shells might grow) from the scan, the same way this bug's actual fix
/// excludes it: this test is about what SHIPS, not about what a test
/// recomputes on purpose to check the shipped code.
fn production_code(source: &str) -> &str {
    match source.find("\nmod tests {") {
        Some(at) => &source[..at],
        None => source,
    }
}

#[test]
fn no_shell_calls_geo_visible_tiles_directly() {
    let shells = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];

    for shell in shells {
        let path = repo_root().join(shell);
        let full_src = read(&path);
        assert!(
            full_src.len() > 2_000,
            "{shell} read as only {} bytes -- suspiciously short for a shell \
             that builds a frame. This test's scan may be reading the wrong \
             file, or the file may have lost its frame-building code; either \
             way, an assertion of zero direct calls below would be worthless.",
            full_src.len(),
        );
        let src = production_code(&full_src);

        let banned = "geo::visible_tiles(";
        let direct = src.matches(banned).count();
        assert_eq!(
            direct, 0,
            "{shell} calls `{banned}` directly {direct} time(s) in its production code; \
             it must go through `render::cover_plan::tile_cover` instead, which asks the \
             globe for its cover before ever falling back to a flat one. A shell deciding \
             this for itself is exactly how the browser drew a globe with half of it left \
             as blank background colour -- see `src/render/cover_plan.rs`'s module doc.",
        );

        let via_plan = src.matches("cover_plan::tile_cover(").count();
        assert!(
            via_plan > 0,
            "{shell} never calls `cover_plan::tile_cover` at all -- it calls neither \
             `geo::visible_tiles` directly (good) nor the shared cover decision (not \
             good), so it may have dropped tile-cover computation entirely rather than \
             routed it through the plan. That would satisfy the assertion above for the \
             wrong reason.",
        );
    }
}

/// Proves `production_code`'s boundary is doing real work, not accidentally
/// excluding everything (which would make the test above pass by scanning
/// nothing) or excluding nothing (which would make it fail on the
/// deliberate direct call described above).
#[test]
fn production_code_excludes_map_views_test_module_but_not_its_real_code() {
    let src = read(&repo_root().join("src/map_view.rs"));
    let production = production_code(&src);

    assert!(
        production.len() < src.len(),
        "production_code found no `mod tests {{` boundary in src/map_view.rs at all -- \
         either the file lost its test module or this scan stopped matching it",
    );
    assert!(
        production.contains("cover_plan::tile_cover("),
        "production_code cut off real code: `MapView::tiles_for_zoom`'s call to \
         `cover_plan::tile_cover` should be well before the test module",
    );
    assert!(
        // The bare identifier also appears in a PRODUCTION comment (`tiles_for_zoom`'s
        // doc, naming this test as the source of the `MAX_VISIBLE_TILES` scenario) --
        // matching `fn ident(` rather than the identifier alone is what tells that
        // mention apart from the test's actual definition.
        !production.contains("fn tilt_clamp_alone_still_allows_a_visible_set_over_the_cap_at_high_zoom("),
        "production_code did not actually exclude the test module -- the boundary string \
         changed or moved",
    );
}

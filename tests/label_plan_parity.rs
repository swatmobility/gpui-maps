//! The browser drew no street names at all, for months, because
//! `web/src/app.rs` was the one shell that built a frame's draw-ready label
//! lists differently from `src/map_view.rs`: it called
//! `symbol_pass::collect_candidates` with `None` for `symbol_pass::GlyphSink`
//! rather than a real one, so every line-placed label was measured, placed
//! along its road and given a collision box, and then rastered nowhere --
//! `symbol_pass::LineLabelPlacement::quads` came back empty on the no-atlas
//! path, and nothing downstream would have collected a non-empty one if it
//! had existed either.
//!
//! `render::label_plan::label_draw_plan` is now the one place that turns
//! `collect_candidates`' output into the three lists a symbol-overlay pass
//! draws (icons, glyphon text, line-placed glyph quads). This test is the
//! reason a shell cannot quietly go back to deriving that split -- or the
//! `GlyphSink` that feeds it -- on its own.
//!
//! iOS used to draw NO labels at all: `ios/src/render/mod.rs` DROPped
//! `glyph_atlas`, `icon_pipeline` and `label_plan` outright, on the grounds
//! that all three named `crate::sprite`, which was outside that crate's
//! module closure (`sprite.rs` reached `reqwest`, a dependency `ios/`
//! exists to avoid). That reason went stale: `sprite.rs`'s two
//! `reqwest`-touching functions are now `cfg`-excluded from `ios/` on their
//! own (a `sprite_http_fetch` Cargo feature, declared and left off in
//! `ios/Cargo.toml` -- see that file and `sprite.rs`'s own doc comments),
//! so the rest of `sprite.rs` -- and therefore `glyph_atlas`/
//! `icon_pipeline`/`label_plan` -- includes cleanly with no `reqwest` in
//! this crate's graph. `ios/src/app.rs` now draws labels the same way
//! native and the browser do, through `Renderer::draw_labels`, so it is
//! held to the same standard here rather than excluded from it.
//!
//! Structured like `tests/space_plan_parity.rs`: read the repo root off
//! `CARGO_MANIFEST_DIR`, and guard against a scan that silently matches
//! nothing passing as success.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Every shell that draws labels. `ios/src/app.rs` joined this list once
/// `render::glyph_atlas`/`render::icon_pipeline`/`render::label_plan`
/// stopped being dropped from that crate's closure -- see this file's own
/// module doc.
const LABEL_DRAWING_SHELLS: [&str; 3] = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];

/// Every label-drawing shell must build its frame's draw-ready label lists
/// through the one shared function rather than re-deriving the
/// place/merge/sort/clip/split sequence itself.
///
/// A shell that stops calling `label_draw_plan` can reintroduce the exact
/// defect this module exists to remove: a partial or diverging copy of the
/// split, with nothing to notice until someone screenshots the wrong
/// platform and finds half the labels (or, as happened, all the street
/// names) missing.
#[test]
fn every_label_drawing_shell_calls_the_shared_label_plan() {
    for shell in LABEL_DRAWING_SHELLS {
        let path = repo_root().join(shell);
        let src = read(&path);

        // A scan of an empty or truncated read would pass the assertion
        // below while checking nothing -- see `space_plan_parity.rs`'s own
        // guard against exactly this.
        assert!(
            src.len() > 2_000,
            "{shell} read as only {} bytes -- suspiciously short for a shell that builds a \
             frame. This test's scan may be reading the wrong file.",
            src.len(),
        );

        assert!(
            src.contains("label_draw_plan("),
            "{shell} never calls `render::label_plan::label_draw_plan` -- it must build this \
             frame's draw-ready instance lists (sprite icons, glyphon text, line-placed glyph \
             quads) through the one shared function rather than re-deriving the \
             place/merge/sort/clip/split sequence itself. That re-derivation is exactly how the \
             browser build stayed a partial reimplementation of `map_view.rs`'s symbol pass \
             instead of a caller of it -- see `src/render/label_plan.rs`'s module doc.",
        );
    }
}

/// The browser's specific historical defect, and the one every shell that
/// is not `src/map_view.rs` (which never had it) must not reintroduce:
/// `collect_candidates` called with a literal `None` where a real
/// `symbol_pass::GlyphSink` belongs, so no line-placed label is ever
/// rastered even though it was placed, measured and collision-boxed.
#[test]
fn every_non_native_shell_supplies_a_real_glyph_sink_to_collect_candidates() {
    for shell in ["web/src/app.rs", "ios/src/app.rs"] {
        let path = repo_root().join(shell);
        let src = read(&path);
        assert!(
            src.len() > 2_000,
            "{shell} read as only {} bytes -- suspiciously short.",
            src.len(),
        );

        assert!(
            src.contains("GlyphSink"),
            "{shell} must construct a `symbol_pass::GlyphSink` backed by its own \
             `render::GlyphAtlas` and hand it to `collect_candidates` -- see that type's doc \
             comment in `src/symbol_pass.rs`.",
        );
        assert!(
            !src.contains("collect_candidates(") || src.contains("Some(&mut glyph_sink)"),
            "{shell} must pass `Some(&mut glyph_sink)` (a real `GlyphSink`) to \
             `collect_candidates`, not `None` -- `None` is what silently left the browser build \
             placing every street name and rastering none of them. See this file's module doc.",
        );
    }
}

//! Every shell picks tiles with the same planar cover.
//!
//! The browser and iOS used to call `cover_plan::flat_cover_scaled`, which
//! has no pitch term, while native called `flat_cover_pitch_aware`, whose
//! circle covered the whole world at 60 degrees. `cover_plan::flat_cover` is
//! the one frustum-trimmed, camera-nearest cover all three use now. This
//! fails the build if the old formulas come back, or if a shell asks any
//! flat cover but that one. Built like `tests/space_plan_parity.rs`.

mod common;

use common::{code_only, read, repo_root, rs_files};

const SHELLS: [&str; 3] = ["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];
const RETIRED: [&str; 2] = ["flat_cover_scaled", "flat_cover_pitch_aware"];

/// Calls to a flat cover OTHER than `cover_plan::flat_cover(` itself, in
/// `source`'s CODE (`code_only` blanks comments and every string/char
/// literal first, so a mention inside either -- e.g. an assertion message
/// naming a retired formula -- is never read as a call):
/// `flat_cover_<anything>(`, including a future third formula.
///
/// Requires a word boundary on BOTH sides of `flat_cover_`, not just after
/// it: `src/map_view.rs` has a test function named
/// `the_flat_cover_is_unchanged_and_the_globe_is_off_above_the_band`, which
/// contains `flat_cover_` as a substring and is followed (once its long name
/// ends) by the `(` of its own `fn ...() {` -- a real false positive this
/// gate hit on today's correct master, not a planted one. Checking only the
/// boundary after the match (as the brief's version does) treats a fn
/// definition's name as a call; checking both sides does not.
fn other_flat_cover_calls(source: &str) -> Vec<String> {
    let stripped = code_only(source);
    let mut out = Vec::new();
    for (number, (code, original)) in stripped.lines().zip(source.lines()).enumerate() {
        let mut from = 0;
        while let Some(at) = code[from..].find("flat_cover_") {
            let start = from + at;
            let ident_end = code[start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(code.len(), |i| start + i);
            let boundary_before = code[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            if boundary_before && code[ident_end..].starts_with('(') {
                out.push(format!("{}: {}", number + 1, original.trim()));
            }
            from = ident_end;
        }
    }
    out
}

#[test]
fn the_scanner_finds_what_it_is_for() {
    let planted = "let t = cover_plan::flat_cover(camera, z).tiles;\n\
                   let u = crate::render::cover_plan::flat_cover_scaled(&c, z);\n\
                   let v = flat_cover_nearest_centre(c, z);\n\
                   // flat_cover_scaled(old) was the browser's\n\
                   fn the_flat_cover_is_unchanged_test() {}\n";
    assert_eq!(other_flat_cover_calls(planted).len(), 2, "{:?}", other_flat_cover_calls(planted));
}

#[test]
fn one_cover_for_every_shell() {
    for shell in SHELLS {
        let source = read(&repo_root().join(shell));
        assert!(source.len() > 2_000, "{shell} read as only {} bytes", source.len());
        let others = other_flat_cover_calls(&source);
        assert!(
            others.is_empty(),
            "{shell} asks a flat cover other than `cover_plan::flat_cover`:\n  {}",
            others.join("\n  ")
        );
        assert!(
            source.contains("cover_plan::flat_cover("),
            "{shell} never calls `cover_plan::flat_cover` -- it calls no other flat cover either, so it \
             may have dropped the planar cover entirely rather than adopted the shared one"
        );
    }
}

#[test]
fn the_retired_formulas_do_not_reappear_anywhere() {
    let mut files = Vec::new();
    for dir in ["src", "web/src", "ios/src"] {
        rs_files(&repo_root().join(dir), &mut files);
    }
    assert!(files.len() >= 50, "found only {} .rs files -- the walk is not reading the tree", files.len());
    let mut hits = Vec::new();
    for file in &files {
        let source = read(file);
        let stripped = code_only(&source);
        // Code only: `code_only` already blanks every comment form
        // (`//`, `///`, `//!`, `/* */`, `/** */`, `/*! */`) and every
        // string/char literal, so the cover module's doc can name both
        // retired formulas for history without tripping this.
        for (number, (code, original)) in stripped.lines().zip(source.lines()).enumerate() {
            for retired in RETIRED {
                if code.contains(retired) {
                    hits.push(format!("{}:{}: {}", file.display(), number + 1, original.trim()));
                }
            }
        }
    }
    assert!(hits.is_empty(), "a retired flat cover is back:\n  {}", hits.join("\n  "));
}

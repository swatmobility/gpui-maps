//! Fails when an engine file the browser compiles reads a clock that a
//! browser does not have.
//!
//! # The defect this exists to catch
//!
//! `std::time::Instant::now()` and `std::time::SystemTime::now()` COMPILE
//! for `wasm32-unknown-unknown`. They abort when they are called:
//!
//! ```text
//! panicked at library/std/src/sys/time/unsupported.rs:13:9:
//! time not implemented on this platform
//! ```
//!
//! `src/symbol_pass.rs` carried one, in `LabelShapeCache::get_or_shape`, on
//! the path every text label takes. The browser build therefore aborted on
//! the first frame that had a font to shape with -- and the page stayed on
//! its boot card, black, forever.
//!
//! Nothing caught it. `cargo check`, `cargo test` and `cargo clippy` are
//! all native, where the call is correct. `./scripts/check-wasm.sh` runs a
//! full `cargo check --target wasm32-unknown-unknown` and returned zero,
//! because the call compiles. A compiler cannot find this one; that is the
//! whole reason for a text scan.
//!
//! # Why one panic was fatal rather than annoying
//!
//! `web/Cargo.toml`'s release profile is `panic = "abort"`, and
//! `wasm32-unknown-unknown` has no unwinding to offer in any case. The trap
//! unwound NOTHING. The frame loop was inside `let mut a = app.borrow_mut()`
//! when it fired, so that `RefMut` was never dropped and the application
//! `RefCell` stayed mutably borrowed for the life of the page. Every
//! callback afterwards aborted in turn on `already borrowed` -- the resize
//! observer, the window `resize` listener, each tile fetch completing, the
//! hidden-tab pump -- one abort per event, forever, each pointing at an
//! innocent call site. The console named five files and none of them was
//! the one with the bug.
//!
//! So the rule is blunt on purpose: these files do not read `std`'s clock.
//! [`gpui_maps::timing::Instant`] is a drop-in for the part of
//! `std::time::Instant` this engine uses, and is backed by
//! `performance.now()` on wasm.
//!
//! # What this does NOT check
//!
//! * `#[cfg(test)]` blocks are scanned like everything else. A test module
//!   is never compiled for wasm, so a clock read in one is harmless -- but
//!   exempting it would mean brace-matching Rust from a regex, and the fix
//!   (`crate::timing::Instant`) costs one word. Strictness is cheaper than
//!   the parser.
//! * `std::thread::sleep`, which is a separate wasm landmine. Every
//!   occurrence in these files today is inside a test, and no non-test code
//!   has any reason to sleep on a render path, so there is nothing yet for
//!   a rule to protect.
//! * Anything the compiler already finds. `./scripts/check-wasm.sh` is
//!   still the real gate for the browser build; this is the half a
//!   compiler is blind to.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `web/` is absent from a packaged `.crate` and present in every checkout.
/// Keyed on the DIRECTORY, so deleting `web/src/lib.rs` fails rather than
/// skips -- the same rule `ios_shell_stays_in_step.rs` applies.
fn web_dir() -> Option<PathBuf> {
    let dir = repo_root().join("web");
    dir.is_dir().then_some(dir)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Every engine file the browser build compiles, as a repo-relative path.
///
/// Read out of `web/src/lib.rs` rather than listed here, so a module added
/// to the browser build comes under this rule the moment it is added and
/// not the next time someone remembers. A `#[path]` naming a `mod.rs`
/// brings its whole directory, because that is what a `mod.rs` reached by
/// `#[path]` does -- see the comment on `style_spec` in that file.
fn engine_files_the_browser_compiles() -> BTreeSet<PathBuf> {
    let root = repo_root();
    let lib = read(&root.join("web/src/lib.rs"));
    let mut out = BTreeSet::new();
    for line in lib.lines() {
        let Some(rest) = line.trim().strip_prefix("#[path = \"") else { continue };
        let Some(rel) = rest.split('"').next() else { continue };
        let Some(engine_rel) = rel.strip_prefix("../../") else { continue };
        let path = root.join(engine_rel);
        assert!(path.is_file(), "web/src/lib.rs names {rel}, which does not exist");
        if path.file_name().is_some_and(|name| name == "mod.rs") {
            collect_rs_files(path.parent().expect("a mod.rs has a parent"), &mut out);
        } else {
            out.insert(path);
        }
    }
    assert!(
        out.len() > 5,
        "found only {} engine files in web/src/lib.rs -- the parser has stopped working, \
         which would make this test pass for the wrong reason",
        out.len()
    );
    out
}

fn collect_rs_files(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("cannot list {}: {err}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.insert(path);
        }
    }
}

/// The two files allowed to name `std`'s clock, and why.
///
/// `src/timing.rs` IS the clock. It is the one place where the wasm arm and
/// the native arm are chosen, so it is the one place `std::time::Instant`
/// may appear -- under `#[cfg(not(target_arch = "wasm32"))]`, which is
/// what makes the rest of this rule enforceable.
///
/// `src/render/solar.rs` wants a WALL clock -- the subsolar point is a
/// function of the date, not of an elapsed span -- and already carries the
/// `cfg(target_arch = "wasm32")` arm for it, reaching `Date.now()` through
/// `js-sys`. Its `SystemTime::now()` sits under
/// `#[cfg(not(target_arch = "wasm32"))]` and is never compiled for the
/// browser. See `solar::now_unix_seconds`.
const ALLOWED: &[&str] = &["src/timing.rs", "src/render/solar.rs"];

/// The code on a line, with a trailing `//` comment removed.
///
/// A whole-line comment yields nothing. A trailing comment is only cut when
/// no quote precedes the `//`, so a `"http://..."` in a doc string or a URL
/// constant is left alone rather than truncated -- truncating it would hide
/// whatever followed on the same line, which is the one way a scan like
/// this can pass for the wrong reason.
fn code_on(line: &str) -> &str {
    let trimmed = line.trim();
    if trimmed.starts_with("//") {
        return "";
    }
    match line.find("//") {
        Some(at) if !line[..at].contains('"') => &line[..at],
        _ => line,
    }
}

/// No engine file the browser compiles reads `std`'s clock.
#[test]
fn no_browser_compiled_engine_file_reads_the_std_clock() {
    if web_dir().is_none() {
        return;
    }
    let root = repo_root();
    let mut offenders: Vec<String> = Vec::new();
    for path in engine_files_the_browser_compiles() {
        let rel = path
            .strip_prefix(&root)
            .expect("every collected file is under the repo root")
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.contains(&rel.as_str()) {
            continue;
        }
        let source = read(&path);
        for (number, line) in source.lines().enumerate() {
            // `Instant::now` and `SystemTime::now` in any spelling: fully
            // qualified, or bare after a `use std::time::Instant`. The
            // engine's own clock is `timing::Instant`, whose `::now` is
            // reached through a path that says so.
            let code = code_on(line);
            let reads_clock = (code.contains("Instant::now") || code.contains("SystemTime::now"))
                && !code.contains("timing::Instant");
            if reads_clock {
                offenders.push(format!("  {rel}:{}  {}", number + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these lines read `std`'s clock in an engine file the browser compiles.\n\
         On wasm32-unknown-unknown they compile and then ABORT the page on the first \
         call -- and with `panic = \"abort\"` the abort leaves the application `RefCell` \
         mutably borrowed forever, so every later callback aborts too.\n\
         Use `crate::timing::Instant` instead; it is a drop-in.\n\n{}\n",
        offenders.join("\n")
    );
}

/// The clock the rule above points at is actually in the browser build.
///
/// Without this, a `web/src/lib.rs` that dropped the `#[path]` include would
/// leave the rule naming a module the browser cannot reach -- and the next
/// person to follow the advice would get a compile error instead of a fix.
#[test]
fn the_browser_build_includes_the_clock_the_rule_points_at() {
    if web_dir().is_none() {
        return;
    }
    let lib = read(&repo_root().join("web/src/lib.rs"));
    assert!(
        lib.contains("../../src/timing.rs"),
        "web/src/lib.rs no longer includes src/timing.rs, so \
         `crate::timing::Instant` -- the fix this file's other test recommends -- \
         does not exist in the browser build"
    );
}

/// `src/render/alloc.rs` is included by the iOS shell as well, and names
/// the clock module too. Its crate root has to carry it.
#[test]
fn the_ios_shell_includes_the_clock_its_engine_files_name() {
    let ios = repo_root().join("ios");
    if !ios.is_dir() {
        return;
    }
    let lib = read(&ios.join("src/lib.rs"));
    assert!(
        lib.contains("../../src/timing.rs"),
        "ios/src/lib.rs does not include src/timing.rs, but ios/src/render/mod.rs \
         includes src/render/alloc.rs, which names `crate::timing::Instant`"
    );
}

/// The story's pure half (story-content 1a, Task 20): every module the
/// browser and the iOS shell must compile so a story's layout, panels,
/// callouts, effects, timeline, tiers and captions are the native ones.
/// Each shell names each by `#[path]`; a module dropped from either
/// include list fails here, by name, rather than as a missing item deep in
/// a shell's own build.
const STORY_PURE_MODULES: &[&str] = &[
    "layout.rs",
    "effects.rs",
    "chapters.rs",
    "captions.rs",
    "furniture.rs",
    "callout.rs",
    "panel.rs",
    "tiers.rs",
    "overlay_layers.rs",
    "timeline.rs",
    "cards.rs",
    "card_layout.rs",
    "frame_layout.rs",
];

/// The `#[path]` targets of `lib`, as written.
fn path_includes(lib: &str) -> BTreeSet<String> {
    lib.lines()
        .filter_map(|line| line.trim().strip_prefix("#[path = \""))
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect()
}

#[test]
fn the_web_and_ios_shells_include_every_pure_story_module() {
    let root = repo_root();
    for shell in ["web", "ios"] {
        if !root.join(shell).is_dir() {
            continue;
        }
        let includes = path_includes(&read(&root.join(shell).join("src/lib.rs")));
        let missing: Vec<&str> = STORY_PURE_MODULES
            .iter()
            .copied()
            .filter(|m| !includes.contains(&format!("../../src/story/{m}")))
            .collect();
        assert!(missing.is_empty(), "{shell}/src/lib.rs does not include src/story/{missing:?}");
        for m in STORY_PURE_MODULES {
            assert!(root.join("src/story").join(m).is_file(), "src/story/{m} is gone: update this list");
        }
    }
}

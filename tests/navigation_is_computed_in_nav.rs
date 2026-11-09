//! Navigation is computed in `src/nav/` and nowhere else.
//!
//! Two rules, both about where code lives rather than what it does, so a
//! text scan is the right instrument (the same reasoning as
//! `tests/camera_is_written_by_the_controller.rs`):
//!
//! 1. `src/nav/` names no platform (`gpui`, `web_sys`, `objc2`, `winit`), no
//!    GPU (`wgpu`, `glyphon`), no clock, no I/O and never prints. The browser
//!    and the phone compile it by `#[path]`, a browser aborts on a clock
//!    read, and a print is how a routing key would reach a log.
//! 2. No shell computes navigation: no shell file names Ferrostar (in any
//!    form -- a plain use, an alias, a bare mention), and no shell file
//!    reaches into `src/nav/` through any path other than `nav::host` (the
//!    `NavHost` wrapper Task 9 builds) or a later chrome/draw entry point
//!    named in `ALLOWED_NAV_PATHS` as a shell is wired onto it. A shell that
//!    called `nav::instructions::compile` or aliased `parse_route` around a
//!    literal-string ban would have grown its own copy of a rule, which is
//!    how three shells drifted apart on the camera -- so this is structural
//!    (every `nav::` path outside the allowlist, not a list of today's
//!    banned function names) rather than a list that the next task can
//!    silently outgrow.
//!
//! Rule 2 scans every `.rs` file physically inside `web/src/` and `ios/src/`
//! (recursively -- a helper module beside `app.rs` is covered, not just
//! `app.rs` itself) plus the native shell files `global.md`'s file
//! structure table lists (`NATIVE_SHELL_FILES`). Files under `src/nav/`
//! itself are untouched by this scan: they are reached by `#[path]`, not
//! physically inside `web/src/` or `ios/src/`, and rule 1 already covers
//! them. Every file this scan expects is asserted to exist, so renaming one
//! is a failure here rather than a silent skip.
//!
//! `WIRED` lists the shells that have been moved onto `NavHost`; the wiring
//! tasks add theirs, and from then on the shell must visibly use it.

mod common;

use common::{code_only, read, repo_root, rs_files};
use std::path::PathBuf;

/// Native shell files the plan wires directly (`global.md`'s file structure
/// table, "Native wiring", Tasks 11 and 14). `src/map_view.rs` is not under
/// a `src/` subtree this scan otherwise recurses into, so it is named here
/// explicitly rather than discovered.
const NATIVE_SHELL_FILES: [&str; 2] = ["src/map_view.rs", "examples/mapbox_streets_demo.rs"];

/// Shells already on `NavHost`. Tasks 11, 12 and 13 each add one.
const WIRED: &[&str] = &["src/map_view.rs", "web/src/app.rs", "ios/src/app.rs"];

const NAV_BANS: [&str; 20] = [
    "gpui",
    "web_sys",
    "objc2",
    "winit",
    "wgpu",
    "glyphon",
    "Instant::now",
    "SystemTime::now",
    "std::env",
    "std::fs",
    "reqwest",
    "ureq",
    "println!",
    "eprintln!",
    "print!(",
    "dbg!(",
    "log::",
    "tracing::",
    "env!(",
    "option_env!(",
];

/// `nav::` paths a shell may reach for without having "computed navigation
/// itself". Today that is just the wrapper Task 9 builds; a later wiring
/// task may add a chrome/draw entry point here as it moves a shell onto it
/// -- see the module doc.
const ALLOWED_NAV_PATHS: [&str; 1] = ["host"];

/// `"<line>: <text>"` for every line of `source`'s CODE (comments and string
/// literals blanked by `code_only`) that contains one of `needles`.
fn offending_lines(source: &str, needles: &[&str]) -> Vec<String> {
    let code = code_only(source);
    code.lines()
        .zip(source.lines())
        .enumerate()
        .filter(|(_, (code, _))| needles.iter().any(|n| code.contains(n)))
        .map(|(number, (_, original))| format!("{}: {}", number + 1, original.trim()))
        .collect()
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True if `code` contains `word` as a whole identifier: not part of a
/// longer name (`ferrostar_helper`) and not a substring inside another one
/// (`myferrostar`). This is what makes `use ferrostar as fs;` and
/// `extern crate ferrostar;` count, not just `ferrostar::`.
fn contains_word(code: &str, word: &str) -> bool {
    let bytes = code.as_bytes();
    let mut start = 0usize;
    while let Some(pos) = code.get(start..).and_then(|rest| rest.find(word)) {
        let at = start + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        let after = at + word.len();
        let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// True if `code` has a whole-word `nav` immediately followed by `::` whose
/// next path segment is not one of `ALLOWED_NAV_PATHS`. Matching the path
/// itself (rather than a list of function names behind it) is what catches
/// `nav::instructions::compile(`, `nav::route_source::parse_route(` and,
/// crucially, an alias import: `use crate::nav::instructions::attach as a;`
/// contains the banned path on the `use` line itself, before the alias ever
/// gets a chance to be called under a different name.
fn has_banned_nav_path(code: &str) -> bool {
    let bytes = code.as_bytes();
    let mut start = 0usize;
    while let Some(pos) = code.get(start..).and_then(|rest| rest.find("nav::")) {
        let at = start + pos;
        let before_ok = at == 0 || !is_ident_byte(bytes[at - 1]);
        if before_ok {
            let after = &code[at + "nav::".len()..];
            let allowed = ALLOWED_NAV_PATHS.iter().any(|allowed_prefix| {
                after
                    .strip_prefix(allowed_prefix)
                    .is_some_and(|rest| rest.as_bytes().first().is_none_or(|&b| !is_ident_byte(b)))
            });
            if !allowed {
                return true;
            }
        }
        start = at + 1;
    }
    false
}

/// `"<line>: <text>"` for every CODE line of `source` that names Ferrostar
/// in any form, or reaches a `nav::` path outside `ALLOWED_NAV_PATHS`.
fn shell_violations(source: &str) -> Vec<String> {
    let code = code_only(source);
    code.lines()
        .zip(source.lines())
        .enumerate()
        .filter(|(_, (code, _))| contains_word(code, "ferrostar") || has_banned_nav_path(code))
        .map(|(number, (_, original))| format!("{}: {}", number + 1, original.trim()))
        .collect()
}

/// Every `.rs` file compiled as shell code: everything physically inside
/// `web/src/` and `ios/src/` (recursively), plus `NATIVE_SHELL_FILES`. Not
/// `src/nav/` itself -- that tree is reached by `#[path]`, and is covered by
/// rule 1 instead.
fn shell_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = Vec::new();
    for dir in ["web/src", "ios/src"] {
        let path = root.join(dir);
        assert!(path.is_dir(), "{dir} is missing -- the shell scan would check nothing");
        rs_files(&path, &mut files);
    }
    for file in NATIVE_SHELL_FILES {
        let path = root.join(file);
        assert!(path.is_file(), "{file} is missing -- the shell scan expects it (see global.md's file structure table)");
        files.push(path);
    }
    files
}

#[test]
fn the_purity_scanner_finds_what_it_is_for() {
    let source = "// gpui in a comment is fine\nlet s = \"std::fs in a string is fine\";\nlet t = std::time::Instant::now();\n";
    assert_eq!(offending_lines(source, &NAV_BANS), ["3: let t = std::time::Instant::now();"]);
}

#[test]
fn the_shell_scanner_rejects_an_alias_a_bare_mention_and_a_banned_nav_path() {
    assert!(!shell_violations("use ferrostar as fs;\n").is_empty(), "a `use ferrostar as fs;` must be caught");
    assert!(
        !shell_violations("crate::nav::route_source::compile(step);\n").is_empty(),
        "a banned nav:: path must be caught"
    );
    assert!(
        !shell_violations("use crate::nav::instructions::attach as a;\n").is_empty(),
        "the alias import line itself carries the banned path and must be caught"
    );
    assert!(shell_violations("crate::nav::host::NavHost::new();\n").is_empty(), "nav::host is the allowed path");
    assert!(shell_violations("// ferrostar is mentioned only in a comment\n").is_empty());
}

#[test]
fn nav_names_no_platform_no_gpu_no_clock_and_does_no_io() {
    let mut files = Vec::new();
    rs_files(&repo_root().join("src/nav"), &mut files);
    assert!(!files.is_empty(), "src/nav has no .rs files -- this gate would check nothing");
    let mut offenders = Vec::new();
    for file in files {
        for line in offending_lines(&read(&file), &NAV_BANS) {
            offenders.push(format!("{}:{line}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "src/nav/ must stay platform-, GPU-, clock- and I/O-free (see src/nav/mod.rs):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn the_browser_and_the_phone_compile_nav_from_the_one_source() {
    for lib in ["web/src/lib.rs", "ios/src/lib.rs"] {
        let path = repo_root().join(lib);
        if !path.parent().is_some_and(|p| p.is_dir()) {
            continue;
        }
        assert!(
            read(&path).contains("#[path = \"../../src/nav/mod.rs\"]"),
            "{lib} does not include src/nav/mod.rs; the shell would have no navigation, or its own"
        );
    }
}

#[test]
fn no_shell_file_reaches_past_nav_host_into_navigation_itself() {
    let mut offenders = Vec::new();
    for file in shell_files() {
        for line in shell_violations(&read(&file)) {
            offenders.push(format!("{}:{line}", file.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "a shell file reaches past `nav::host` into navigation itself (see ALLOWED_NAV_PATHS):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn every_wired_shell_drives_navigation_through_nav_host() {
    for shell in WIRED {
        let code = code_only(&read(&repo_root().join(shell)));
        for needle in ["NavHost", "nav.advance(", "nav.scene(", "nav.collision_boxes(", "nav_layer::sync_route("] {
            assert!(code.contains(needle), "{shell} is wired but never calls `{needle}`");
        }
    }
}

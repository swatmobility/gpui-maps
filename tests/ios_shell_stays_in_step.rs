//! Fails when the iOS shell and the engine drift apart, without needing an
//! iOS toolchain.
//!
//! # Why this file exists
//!
//! `ios/` declares its own `[workspace]` (see the comment at the top of
//! `ios/Cargo.toml`) so that its iOS-only dependency set never reaches the
//! shipped crate's lockfile. The cost of that detachment is that NO command
//! run from the repository root has ever compiled it: `cargo check -p
//! swat-maps-ios` from here answers
//!
//!     error: package ID specification `swat-maps-ios` did not match any packages
//!
//! so `cargo check`, `cargo test` and `cargo clippy` have all been green
//! while the iOS build carried eleven errors through a tagged release
//! (`29453cd`, v0.6.2). Nobody was careless; there was simply nothing to
//! notice. `web/` had the identical defect on the identical day.
//!
//! # What this catches, and what it does not
//!
//! `scripts/check-ios.sh` is the real check -- it runs the compiler, and
//! nothing but a compiler catches a changed function signature or a `cfg`
//! the iOS target fails. But it needs the `aarch64-apple-ios` target
//! installed and a few minutes, so it is a push-time check rather than an
//! edit-time one.
//!
//! This is the cheap, early half: it reads two module lists as text and
//! fails when they disagree. It costs a handful of file reads, needs no
//! toolchain, and runs inside the `cargo test` every contributor already
//! makes. It is deliberately about the one drift a glob re-export cannot
//! absorb -- the engine growing or losing a `render` FILE -- because the
//! name-level drift that caused all eleven errors is gone by construction
//! now (`ios/src/render/mod.rs` forwards with globs; see its module
//! comment).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `ios/` is absent from a packaged `.crate` and present in every checkout.
/// Skipping when it is absent is the only way these tests can be honest in
/// both places -- and the skip is narrow: it is keyed on the DIRECTORY not
/// existing at all, not on any file inside it being missing, so deleting
/// `ios/src/render/mod.rs` still fails rather than skips.
fn ios_dir() -> Option<PathBuf> {
    let dir = repo_root().join("ios");
    dir.is_dir().then_some(dir)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Every `mod` a module-list file declares, whatever its visibility or its
/// attributes.
///
/// Deliberately a text scan and not a parse: both files this reads are
/// module lists with one declaration per line, and a scanner that cannot
/// understand anything cleverer is one that says so loudly -- via the
/// emptiness assertions below -- rather than quietly matching less.
fn declared_modules(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        let rest = line
            .strip_prefix("pub mod ")
            .or_else(|| line.strip_prefix("pub(crate) mod "))
            .or_else(|| line.strip_prefix("mod "));
        let Some(rest) = rest else { continue };
        let Some(name) = rest.strip_suffix(';') else { continue };
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// The engine `render` submodules the iOS shell records as deliberately
/// absent, read from its own `DROPPED: <name> -- <reason>` lines.
///
/// Parsed out of the shell rather than restated here on purpose: a list of
/// exceptions kept in a different file from the thing it excepts is the
/// same defect this whole test exists about.
fn dropped_modules(shell_source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in shell_source.lines() {
        let line = line.trim().trim_start_matches("//!").trim_start_matches("//").trim();
        let Some(rest) = line.strip_prefix("DROPPED:") else { continue };
        let rest = rest.trim();
        let Some((name, reason)) = rest.split_once("--") else {
            panic!(
                "malformed drop line {line:?} in the iOS render shim: the shape is \
                 `DROPPED: <name> -- <reason>`, and the reason is the point of the line"
            );
        };
        let name = name.trim();
        assert!(
            reason.trim().len() >= 20,
            "the drop line for {name:?} in the iOS render shim gives no real reason. A \
             module dropped without one is a decision nobody can review or reverse."
        );
        out.insert(name.to_string());
    }
    out
}

/// The iOS shell must account for every engine `render` submodule -- by
/// including it, or by recording why it does not.
///
/// The engine gaining a file is the one drift the shell's glob re-exports
/// cannot absorb, and it is the drift that stays invisible longest: a new
/// module only breaks iOS once something already in the closure calls into
/// it, which can be a release or two after it lands.
#[test]
fn every_engine_render_module_is_either_included_by_ios_or_declared_dropped() {
    let Some(ios) = ios_dir() else { return };
    let engine = repo_root().join("src/render/mod.rs");
    let shell = ios.join("src/render/mod.rs");

    let engine_modules = declared_modules(&read(&engine));
    let shell_source = read(&shell);
    let shell_modules = declared_modules(&shell_source);
    let dropped = dropped_modules(&shell_source);

    // A scan that matched nothing would pass every assertion below while
    // checking nothing -- the same class of defect it exists to catch.
    assert!(
        engine_modules.len() >= 10,
        "expected `src/render/mod.rs` to declare at least ten modules, found {} ({:?}) -- \
         the scanner in this test has stopped matching the file it reads",
        engine_modules.len(),
        engine_modules,
    );
    assert!(
        shell_modules.len() >= 4,
        "expected {} to declare at least four modules, found {} ({:?})",
        shell.display(),
        shell_modules.len(),
        shell_modules,
    );

    let unaccounted: Vec<&String> = engine_modules
        .iter()
        .filter(|name| !shell_modules.contains(*name) && !dropped.contains(*name))
        .collect();
    assert!(
        unaccounted.is_empty(),
        "`src/render/mod.rs` declares {unaccounted:?}, which {} neither includes nor records \
         as dropped. `ios/` is its own cargo workspace and no command from this root \
         compiles it, so this test is the only place the addition surfaces. Either add a \
         `#[path]` for it there, or add a `DROPPED: <name> -- <reason>` line saying why \
         not. Then run `./scripts/check-ios.sh`.",
        shell.display(),
    );

    let stale: Vec<&String> = dropped
        .iter()
        .filter(|name| !engine_modules.contains(*name))
        .collect();
    assert!(
        stale.is_empty(),
        "{} records {stale:?} as dropped, but `src/render/mod.rs` no longer declares them. A \
         decision recorded about a module that does not exist is noise; remove the line.",
        shell.display(),
    );

    let both: Vec<&String> = shell_modules.intersection(&dropped).collect();
    assert!(
        both.is_empty(),
        "{} both includes and records as dropped: {both:?}. One of the two is wrong.",
        shell.display(),
    );

    let phantom: Vec<&String> = shell_modules
        .iter()
        .filter(|name| !engine_modules.contains(*name))
        .collect();
    assert!(
        phantom.is_empty(),
        "{} declares {phantom:?}, which `src/render/mod.rs` does not. Every module in the \
         iOS render shim is meant to be a `#[path]` at engine source; a name that exists \
         only there is a fork.",
        shell.display(),
    );
}

/// Every `#[path]` in the iOS crate must name a file that exists.
///
/// A renamed or moved engine file breaks the iOS build and nothing native
/// notices -- `scripts/check-ios.sh` is the only thing that compiles those
/// includes, and it is a separate command. This is the half of that failure
/// a plain `cargo test` can catch.
#[test]
fn every_path_include_in_the_ios_crate_names_a_file_that_exists() {
    let Some(ios) = ios_dir() else { return };
    // The two hand-written module files that carry `#[path]` attributes
    // (`overlay` is the engine's own `src/overlay/mod.rs` now, included
    // whole). `app.rs`, `net.rs`, `styles.rs` and `display_link.rs` have
    // none -- they are the shell's own code, not includes.
    let mut checked = 0usize;
    for relative in ["src/lib.rs", "src/render/mod.rs"] {
        let file = ios.join(relative);
        let source = read(&file);
        // The directory a non-inline `#[path]` resolves against is the one
        // holding the mod-rs file that declares it -- the same rule the
        // includes themselves are written to.
        let base = file.parent().expect("a file has a parent").to_path_buf();
        for line in source.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix(r#"#[path = ""#) else { continue };
            let Some(target) = rest.strip_suffix(r#""]"#) else { continue };
            // `#[path = "."]` on an inline module (`pub mod story { .. }`)
            // points its children's includes back at this directory; it
            // names no file. The children are checked as lines below.
            if target == "." {
                continue;
            }
            let resolved = normalise(&base.join(target));
            assert!(
                resolved.is_file(),
                "{}: `#[path = \"{target}\"]` resolves to {}, which does not exist. An engine \
                 file was renamed or moved and the include did not follow; the iOS build is \
                 broken. `./scripts/check-ios.sh` shows the rest.",
                file.display(),
                resolved.display(),
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 10,
        "expected the iOS crate to carry at least ten `#[path]` includes, found {checked} -- \
         either the scanner above stopped matching or the crate stopped reaching the engine \
         by source",
    );
}

/// The iOS render shim must forward with globs, not with a hand-written
/// list.
///
/// It carried a hand-written list for four releases and the list drifted:
/// all eleven errors at `29453cd` were three names the engine had and it
/// had lost. Whatever a future restatement is brought back for, it puts
/// that failure back.
#[test]
fn the_ios_render_shim_forwards_by_glob_rather_than_restating_names() {
    let Some(ios) = ios_dir() else { return };
    let shell = ios.join("src/render/mod.rs");
    let source = read(&shell);

    // Only real code lines count. Searching the whole file as one string
    // would let a COMMENTED-OUT glob satisfy the assertion -- measured:
    // commenting out `pub use alloc::*;` left this test green while
    // `./scripts/check-ios.sh` produced all eleven of the original errors.
    // A guard that a one-character edit can fool is not a guard.
    let code: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .collect();

    for module in ["alloc", "camera", "gpu", "mesh"] {
        let glob = format!("pub use {module}::*;");
        assert!(
            code.contains(&glob.as_str()),
            "{} must forward `{module}` with `{glob}`, as a live line rather than a \
             comment. A hand-written re-export list is what put eleven errors into \
             v0.6.2; see that file's module comment.",
            shell.display(),
        );
    }

    // A named re-export from an included engine module is the restatement
    // coming back one item at a time, which is how it arrived last time.
    for line in &code {
        for module in ["alloc", "camera", "gpu", "mesh"] {
            let named = format!("use {module}::{{");
            assert!(
                !line.contains(&named),
                "{}: `{line}` re-exports names from `{module}` one by one. That list drifts \
                 -- forward the module with `pub use {module}::*;` instead, and let an item \
                 added to the engine arrive here on its own.",
                shell.display(),
            );
        }
    }
}

/// `Path::canonicalize` would do this, but it also resolves symlinks and
/// fails outright on a missing file -- and a missing file is exactly what
/// the caller wants to report on rather than panic over.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

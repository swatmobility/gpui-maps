//! Test-only access to the Mapbox-authored style documents.
//!
//! Seven style documents written by Mapbox (or derived from Mapbox Studio
//! styles) are kept out of the public tree: `mapbox-style.json`,
//! `mapbox-streets-style.json`, `mapbox-standard-dark-style.json`,
//! `mapbox-standard-dark-reference-style.json`, `mapbox-light-style.json`,
//! `swat-routes-dark-style.json` and `swat-biz-light-style.json`. Tests that
//! read them look in the directory named by `MAPBOX_FIXTURES` (populate it
//! with `scripts/fetch-mapbox-styles.sh`), by file name only. When the
//! variable is unset, or the file is missing from it, the test prints why
//! and returns early, so a public checkout runs `cargo test --lib` green
//! without any Mapbox content.

use std::path::PathBuf;

/// The environment variable naming the fixtures directory.
pub(crate) const ENV: &str = "MAPBOX_FIXTURES";

/// `$MAPBOX_FIXTURES/<file name of name>` if the variable is set and the
/// file exists. `name` may carry a directory (`examples/x.json`); only its
/// file name is used.
pub(crate) fn path(name: &str) -> Option<PathBuf> {
    let file = std::path::Path::new(name).file_name()?.to_owned();
    let Some(dir) = std::env::var_os(ENV).filter(|d| !d.is_empty()) else {
        eprintln!("skipped: {name} is a Mapbox fixture and {ENV} is unset (see scripts/fetch-mapbox-styles.sh)");
        return None;
    };
    let p = PathBuf::from(dir).join(file);
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("skipped: {} does not exist ({ENV} is set, but lacks {name})", p.display());
        None
    }
}

/// The fixture's bytes, or `None` (with a printed reason) when absent.
pub(crate) fn read(name: &str) -> Option<Vec<u8>> {
    path(name).map(|p| std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
}

/// The bytes of a Mapbox fixture, or `return` from the enclosing test.
macro_rules! mapbox_fixture_or_skip {
    ($name:expr) => {
        match $crate::mapbox_fixtures::read($name) {
            Some(bytes) => bytes,
            None => return,
        }
    };
}

/// [`mapbox_fixture_or_skip`] as a `String`.
macro_rules! mapbox_fixture_str_or_skip {
    ($name:expr) => {
        String::from_utf8(mapbox_fixture_or_skip!($name)).expect("a Mapbox fixture is UTF-8")
    };
}

/// Unwrap an `Option` from a fixture-reading helper (one that returns
/// `Option<T>` because its fixture may be absent), or `return` from the
/// enclosing test. The helper has already printed why.
macro_rules! or_skip {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => return,
        }
    };
}

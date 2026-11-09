//! Stages the one Mapbox-authored style this shell can embed.
//!
//! `mapbox-streets-style.json` is Mapbox's document and is not in the public
//! tree. When `MAPBOX_FIXTURES` names a directory holding it (populate one
//! with `scripts/fetch-mapbox-styles.sh`), it is copied into `OUT_DIR` and
//! `styles::MAPBOX_STYLES` embeds it. Otherwise an empty file is written:
//! the composite tile URL then cannot be derived, and the app runs on
//! OpenFreeMap exactly as it does without a token.

use std::path::PathBuf;

fn main() {
    const NAME: &str = "mapbox-streets-style.json";
    println!("cargo:rerun-if-env-changed=MAPBOX_FIXTURES");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join(NAME);
    let src = std::env::var_os("MAPBOX_FIXTURES").filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join(NAME));
    let bytes = match &src {
        Some(p) if p.is_file() => {
            println!("cargo:rerun-if-changed={}", p.display());
            std::fs::read(p).expect("read the Mapbox fixture")
        }
        _ => Vec::new(),
    };
    std::fs::write(&out, bytes).expect("write the staged Mapbox style");
}

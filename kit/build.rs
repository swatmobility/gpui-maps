//! Embeds the repository files the kit's panels and Studio read at run
//! time (vendored styles, thumbnails, previews, small fixtures), so a
//! binary copied out of its build tree -- a release `.app`, or a binary
//! whose worktree was removed -- still has them. `kit::asset_bytes` and
//! `kit::asset` serve them; see `kit/src/kit.rs`.
//!
//! The generated table maps each file's path from the repository root
//! (the same `rel` the code names, e.g. `demos/positron-style.json`) to an
//! `include_bytes!` of it, plus a digest of every byte so a materialised
//! copy on disk is never a stale one.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};

/// What is embedded: `(directory from the repository root, which files,
/// whether it must exist)`. `Top(suffix)` takes that directory's own files
/// ending in `suffix`; `Tree` takes every file under it.
enum Pick {
    Top(&'static str),
    Tree,
    One(&'static str),
}

const EMBED: &[(&str, Pick, Need)] = &[
    // Every vendored style a panel or Studio's basemap menu compiles. The
    // Mapbox and SWAT style JSONs among them are not redistributable and
    // are absent from the open library tree (`scripts/split-manifest.txt`
    // marks them X); `scripts/fetch-mapbox-styles.sh` fetches them.
    ("demos", Pick::Top("-style.json"), Need::Always),
    ("examples", Pick::Top("-style.json"), Need::Always),
    // The keyless look-alikes (`KeylessStyle`): the map-options menu's
    // Streets/Dark (open) and every Mapbox-named page without a token.
    ("examples/styles-open", Pick::Top("-style.json"), Need::Always),
    // The Explorer's sample cards and the Kepler grid's bundled previews.
    ("examples/studio/sample-thumbs", Pick::Tree, Need::StudioTree),
    ("examples/studio/previews", Pick::Tree, Need::StudioTree),
    // "On iPhone & iPad": real iOS captures, on their own budget
    // (`mobile_chapter::CHAPTER_MAX_BYTES`).
    ("examples/studio/mobile-shots", Pick::Tree, Need::StudioTree),
    // Bus routes, the night race's circuits, their SOURCE notes.
    ("examples/data/bus-routes", Pick::Tree, Need::StudioTree),
    ("examples/data/night-race", Pick::Tree, Need::StudioTree),
    ("examples/data/vehicle-gps", Pick::One("SOURCE.md"), Need::StudioTree),
    // The coding agents' guide that Studio's "SDK docs" opens
    // (`overview::docs_target`), so a copied binary has it too.
    ("docs", Pick::One("claude-code.md"), Need::StudioTree),
    // The nav drive's demo routes (`--nav-route NAME`).
    ("tests/fixtures/routes", Pick::Top(".json"), Need::Always),
];

/// Whether an `EMBED` entry may be missing.
#[derive(PartialEq)]
enum Need {
    /// In every tree; a missing one is a build error.
    Always,
    /// Studio's data (`examples/studio/`, `examples/data/`). Present in the
    /// monorepo and the Studio repo; absent from the open library tree,
    /// where nothing reads it. It is required whenever `examples/studio/`
    /// exists, so a rename there still fails the build.
    StudioTree,
}

/// Vendored styles no shipped code opens (reference copies and fixtures
/// for the library's own tests): not embedded.
const NOT_SHIPPED: &[&str] = &["examples/mapbox-style.json", "examples/mapbox-standard-dark-reference-style.json", "examples/swat-biz-light-style.json"];

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let root = manifest.parent().expect("kit/ sits in the repository root").to_path_buf();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let studio_tree = root.join("examples/studio").is_dir();
    println!("cargo:rerun-if-changed={}", root.join("examples/studio").display());
    let excluded = if std::env::var_os("CARGO_FEATURE_INTERNAL_ASSETS").is_some() { Vec::new() } else { excluded_pages(&root) };
    for (dir, pick, need) in EMBED {
        let abs = root.join(dir);
        println!("cargo:rerun-if-changed={}", abs.display());
        if *need == Need::StudioTree && !studio_tree {
            continue;
        }
        match pick {
            Pick::Top(suffix) => {
                for e in std::fs::read_dir(&abs).unwrap_or_else(|e| panic!("{}: {e}", abs.display())).flatten() {
                    let p = e.path();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if p.is_file() && name.ends_with(suffix) && !NOT_SHIPPED.contains(&format!("{dir}/{name}").as_str()) {
                        files.push((format!("{dir}/{name}"), p));
                    }
                }
            }
            Pick::Tree => walk(&root, &abs, &mut files),
            Pick::One(name) => files.push((format!("{dir}/{name}"), abs.join(name))),
        }
    }
    // A free build carries no picture of a page it does not have.
    files.retain(|(rel, _)| !is_excluded_picture(rel, &excluded));
    files.sort();
    let mut digest = DefaultHasher::new();
    let mut out = String::from("/// Generated by kit/build.rs: `(path from the repository root, bytes)`.\npub static EMBEDDED: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in &files {
        println!("cargo:rerun-if-changed={}", abs.display());
        let bytes = std::fs::read(abs).unwrap_or_else(|e| panic!("{}: {e}", abs.display()));
        rel.hash(&mut digest);
        bytes.hash(&mut digest);
        out.push_str(&format!("    ({rel:?}, include_bytes!({:?})),\n", abs.display().to_string()));
    }
    out.push_str("];\n");
    out.push_str(&format!("/// Files in the embedded directories left out on purpose.\npub static NOT_SHIPPED: &[&str] = &{NOT_SHIPPED:?};\n"));
    out.push_str(&format!("/// A digest of every embedded path and byte.\npub const DIGEST: &str = \"{:016x}\";\n", digest.finish()));
    let dest = Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("embedded_assets.rs");
    std::fs::write(dest, out).expect("write the asset table");
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())).flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, files);
        } else if p.is_file() {
            let rel = p.strip_prefix(root).expect("under the root").to_string_lossy().replace('\\', "/");
            files.push((rel, p));
        }
    }
}

/// The gallery pages a free build leaves out, as the file stems of their
/// pictures: the ids in `registry.rs`'s FREE-STUDIO-EXCLUDED list (the
/// list `scripts/check-free-studio.sh` reads too), plus the stems
/// `overview::preview_stem` maps internal-only pages to. Empty outside the
/// Studio tree, where nothing embeds a picture.
fn excluded_pages(root: &Path) -> Vec<String> {
    let registry = root.join("examples/studio/registry.rs");
    let overview = root.join("examples/studio/overview.rs");
    println!("cargo:rerun-if-changed={}", registry.display());
    println!("cargo:rerun-if-changed={}", overview.display());
    let Ok(src) = std::fs::read_to_string(&registry) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    for line in src.lines() {
        if line.contains("FREE-STUDIO-EXCLUDED-BEGIN") {
            inside = true;
        } else if line.contains("FREE-STUDIO-EXCLUDED-END") {
            break;
        } else if inside {
            if let Some(id) = line.trim_start().strip_prefix("(\"").and_then(|r| r.split('"').next()) {
                out.push(id.to_string());
            }
        }
    }
    assert!(!out.is_empty(), "kit/build.rs: no FREE-STUDIO-EXCLUDED ids in {}", registry.display());
    if let Ok(ov) = std::fs::read_to_string(&overview) {
        if let Some(line) = ov.lines().find(|l| l.trim_start().starts_with("const INTERNAL: &[(&str, &str)]")) {
            // `("page-id", "stem")` pairs: keep the stem of each excluded id.
            let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
            for pair in quoted.chunks(2) {
                if let [id, stem] = pair {
                    if out.iter().any(|o| o == id) {
                        out.push((*stem).to_string());
                    }
                }
            }
        }
    }
    out
}

/// Whether `rel` is a preview or sample thumbnail of an excluded page.
fn is_excluded_picture(rel: &str, excluded: &[String]) -> bool {
    let Some(name) = rel.strip_prefix("examples/studio/previews/").or_else(|| rel.strip_prefix("examples/studio/sample-thumbs/")) else { return false };
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    excluded.iter().any(|e| e == stem)
}

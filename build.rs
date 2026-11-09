use std::path::Path;

fn main() {
    let sha = git_sha();
    if let Some(sha) = &sha {
        println!("cargo:rustc-env=GPUI_MAP_GIT_SHA={sha}");
    }
    // The examples' window titles and `examples/common/agentic.rs` read
    // `GPUI_MAPS_BUILD_NUMBER` with `env!`. Here it is the short SHA
    // (`dev` outside this crate's own checkout), not a counter.
    // TODO(split): drop it once those read it with `option_env!`.
    let build = sha.as_deref().map_or_else(
        || "dev".to_string(),
        |sha| {
            let (hash, dirty) = sha.split_once('-').map_or((sha, ""), |(h, _)| (h, "-dirty"));
            format!("{}{dirty}", &hash[..hash.len().min(8)])
        },
    );
    println!("cargo:rustc-env=GPUI_MAPS_BUILD_NUMBER={build}");
}

/// `GPUI_MAP_GIT_SHA`: the commit this build compiled, `-dirty` when a
/// tracked file differed from it. Deliberately no `rerun-if-changed`: any
/// such line would stop cargo rerunning this script on every package file
/// change. As it is, the script reruns whenever the sources change, so the
/// SHA is the one the compiled code came from; a commit made after the
/// build leaves the binary saying `<old sha>-dirty`, which is exactly what
/// it is.
///
/// No build counter here: one writes into the source folder, which
/// `cargo publish` rejects and which dirties every checkout. The Studio's
/// `studio-app/build.rs` keeps its own `.build_number` counter.
///
/// Only THIS crate's own repository counts: git runs in the manifest
/// directory, and the SHA is emitted only when that directory is the
/// work tree's top level. Vendored or unpacked inside another repository
/// (a consumer's `vendor/`, a registry checkout under a repo), the crate
/// records no SHA rather than the enclosing repository's.
fn git_sha() -> Option<String> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(std::path::PathBuf::from)?;
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&manifest_dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    let own_repo = git(&["rev-parse", "--show-toplevel"]).is_some_and(|top| same(Path::new(&top), &manifest_dir));
    if !own_repo {
        return None;
    }
    let sha = git(&["rev-parse", "HEAD"]).filter(|s| !s.is_empty())?;
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    Some(format!("{sha}{}", if dirty { "-dirty" } else { "" }))
}

//! Spec §8.3: a story rendered in a child process (`examples/story`,
//! `--format hashes`) gives the same frame hashes as the same story
//! rendered in this process, through `examples/story`'s
//! `--offline --fixture --format hashes` (one `n sha256` line per frame).
//!
//! The child runs with `GPUI_MAP_BASEMAP_COMPACT(_STROKES)` removed from its
//! environment: the player pins both, and this keeps a developer's shell
//! from making the comparison vacuous or flaky if it ever stopped doing so.
//!
//! Ignored, as spec §8.3 has it: the child is a `cargo run` (a cold build
//! of the example can take minutes). The capture gate runs it:
//! `cargo test --release --test story_determinism -- --ignored`. The child
//! is built in the test's own profile (`--release` only when the test is).
#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::sync::Arc;

struct Hashes(Vec<String>);
impl gpui_maps::story::FrameSink for Hashes {
    fn push(&mut self, _: usize, f: &gpui_maps::capture::Capture) -> anyhow::Result<()> {
        self.0.push(f.sha256_hex());
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

#[test]
#[ignore = "spec §8.3: spawns `cargo run --example story` (minutes cold); the capture gate runs it: cargo test --release --test story_determinism -- --ignored"]
fn a_child_process_render_matches_the_in_process_hashes() {
    let root = env!("CARGO_MANIFEST_DIR");
    let raw = std::fs::read(format!("{root}/tests/fixtures/stories/det.json")).unwrap();
    let script = gpui_maps::story::parse(&raw).unwrap();
    let style: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{root}/examples/liberty-style.json")).unwrap()).unwrap();
    let inputs = gpui_maps::story::SceneInputs {
        source: Arc::new(gpui_maps::story::fixture::FixtureSource),
        source_label: "fixture".into(),
        base_style: style,
        load_style: Box::new(|_| None),
        overlays: HashMap::new(),
        tracks: gpui_maps::story::tracks::TrackSet::default(),
        label_fonts: Some(gpui_maps::story::shipped_label_fonts()),
        flat_tile_zoom_max: 14,
        raster: None,
        credits: Vec::new(),
        wordmark: None,
        sprites: None,
        sprite_sheet: None,
        sprite_notes: Vec::new(),
        story_dir: std::path::PathBuf::new(),
        offline: true,
        images_validated: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let opts = gpui_maps::story::RenderOptions { cache_dir: dir.path().into(), ..Default::default() };
    let mut sink = Hashes(Vec::new());
    gpui_maps::story::render(&script, &raw, inputs, opts, &mut sink, &mut |_| {}).unwrap();

    // The child is built in THIS test's profile, so it reuses the warm
    // artifacts rather than starting a cold build of the other profile.
    let mut args = vec!["run"];
    if !cfg!(debug_assertions) {
        args.push("--release");
    }
    args.extend(["--example", "story", "--", "tests/fixtures/stories/det.json", "--offline", "--fixture", "--format", "hashes"]);
    let out = std::process::Command::new(env!("CARGO"))
        .current_dir(root)
        .env_remove("GPUI_MAP_BASEMAP_COMPACT")
        .env_remove("GPUI_MAP_BASEMAP_COMPACT_STROKES")
        .args(&args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // `n sha256` per frame; anything else on stdout is not a frame line.
    let child: Vec<(usize, String)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let (n, hash) = l.split_once(' ')?;
            (hash.len() == 64).then(|| (n.parse().ok(), hash.to_string())).and_then(|(n, h)| Some((n?, h)))
        })
        .collect();
    let numbers: Vec<usize> = child.iter().map(|(n, _)| *n).collect();
    assert_eq!(numbers, (0..sink.0.len()).collect::<Vec<_>>(), "one line per frame, in order");
    assert!(sink.0.len() >= 20, "det.json is 2 s at 10 fps: {} frames", sink.0.len());
    let hashes: Vec<String> = child.into_iter().map(|(_, h)| h).collect();
    assert_eq!(hashes, sink.0);
}

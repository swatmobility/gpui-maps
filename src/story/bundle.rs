//! The render bundle (spec §6): beside the video, a chapter still each, a
//! poster, the caption track as SRT and VTT, `transcript.html`, a copy of
//! every image the story drew, and `post-description.txt` (spec D2, R10).
//!
//! ```text
//! <dir>/
//!   media/<sha256[..16]>.<ext>   each loaded image's ORIGINAL bytes
//!   stills/<chapter>.png         each chapter's frame at its `still_t`
//!   poster.png                   the first chapter's still, or `output.poster_at`'s frame
//!   <stem>.srt  <stem>.vtt       the one caption track (`captions::track`)
//!   transcript.html              the "read as article" page (`transcript`)
//!   post-description.txt         every credit line, the tile credit last (`post_description_lines`)
//! ```
//!
//! The MP4 and the `<stem>.story.json` manifest are the caller's, around
//! this (the `story` CLI; Task 17 adds the manifest's bundle rows).
//!
//! Every file is written whole or not at all: through a temporary name in
//! the same folder and a rename, so a crash or a full disk never leaves a
//! truncated still under its final name. Writing into a folder that
//! already holds a bundle replaces each file this one writes (the same
//! rule as `--out` and a `--frames` re-render: the newest render wins)
//! and leaves any other file alone. Every name is chosen here -- a
//! content hash or a sanitised chapter id -- so no script string can
//! steer a path outside `dir`, and every text file's author strings pass
//! [`crate::story::redact_url_secrets`].
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing))]

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::capture::Capture;
use crate::story::captions::{self, Cue};
use crate::story::chapters::Chapter;
use crate::story::media::LoadedImage;
use crate::story::player::Player;
use crate::story::transcript;

/// Where a bundle goes and what it is called.
#[derive(Clone, Debug)]
pub struct BundleOptions {
    /// The bundle folder (created when missing).
    pub dir: PathBuf,
    /// The SRT/VTT (and the caller's MP4 and manifest) file stem: no path
    /// separator, not `.` or `..`.
    pub stem: String,
    /// Whether the caller writes a video beside the bundle (false on wasm
    /// builds and for `--format png`): recorded, nothing here encodes.
    pub video: bool,
    /// Write inside a git work tree anyway (`--allow-repo-output`); off,
    /// such a `dir` is refused ([`crate::story::burn_in::refuse_repo_output`]).
    pub allow_repo_output: bool,
}

/// What [`write_bundle`] wrote.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BundleReport {
    /// Every file, in the order written.
    pub files: Vec<PathBuf>,
    /// `(chapter id, still path)`, in time order.
    pub stills: Vec<(String, PathBuf)>,
    pub poster: PathBuf,
    /// `(redacted src, copy path)` per image, in load order.
    pub media: Vec<(String, PathBuf)>,
}

impl BundleReport {
    /// Record this bundle in `manifest`: each chapter's still path (by
    /// chapter id) and the poster.
    pub fn record_in(&self, manifest: &mut crate::story::RenderManifest) {
        for (chapter, still) in &mut manifest.chapters {
            *still = self.stills.iter().find(|(id, _)| *id == chapter.id).map(|(_, p)| p.clone());
        }
        manifest.poster = Some(self.poster.clone());
    }
}

/// Write the bundle into `opts.dir`, in this order: `media/` (a copy of
/// every [`LoadedImage`]'s original bytes, content-addressed), then
/// `stills/<chapter>.png` at each chapter's `still_t`, `poster.png` (the
/// first chapter's still byte for byte, or the frame at `output.poster_at`),
/// `<stem>.srt`, `<stem>.vtt`, `transcript.html` and, last,
/// `post-description.txt` ([`post_description_lines`]).
///
/// Stills are frames of the video ([`Player::render_still_at`]): the same
/// bytes a full render gives that frame.
///
/// # Errors
/// `dir` inside a git work tree (unless `allow_repo_output`); a `stem`
/// that is empty, `.`/`..` or holds a path separator; a render or write
/// failure (naming the file). Files already written stay; none is partial.
pub fn write_bundle(
    player: &mut Player,
    chapters: &[Chapter],
    cues: &[Cue],
    images: &[LoadedImage],
    opts: &BundleOptions,
) -> anyhow::Result<BundleReport> {
    check_stem(&opts.stem)?;
    crate::story::burn_in::refuse_repo_output(&opts.dir, opts.allow_repo_output)?;
    let dir = &opts.dir;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut report = BundleReport::default();

    // media/: each distinct image once, under a name from its own bytes.
    let media = dir.join("media");
    for image in images {
        let path = media.join(transcript::media_name(image));
        if !report.files.contains(&path) {
            write_atomic(&path, &image.source)?;
            report.files.push(path.clone());
        }
        report.media.push((crate::story::redact_url_secrets(&image.src), path));
    }

    // stills/: in time order, so the first written is the poster's.
    let stems = transcript::still_stems(chapters);
    let mut first_still: Option<Vec<u8>> = None;
    for index in transcript::time_order(chapters) {
        let (Some(chapter), Some(stem)) = (chapters.get(index), stems.get(index)) else { continue };
        let frame = player.render_still_at(chapter.still_t).with_context(|| format!("rendering chapter `{}`'s still", chapter.id))?;
        let png = encode_png(&frame)?;
        let path = dir.join("stills").join(format!("{stem}.png"));
        write_atomic(&path, &png)?;
        report.files.push(path.clone());
        report.stills.push((chapter.id.clone(), path));
        first_still.get_or_insert(png);
    }

    // poster.png: `poster_at`'s frame, else the first chapter's still.
    let poster_png = match (player.script().output.poster_at, first_still) {
        (Some(at), _) => encode_png(&player.render_still_at(at).context("rendering the poster")?)?,
        (None, Some(png)) => png,
        // No chapter at all (a caller's empty list): the middle of the story.
        (None, None) => {
            let mid = player.frames() / 2;
            let t = mid as f64 / f64::from(player.output().fps.max(1));
            encode_png(&player.render_still_at(t).context("rendering the poster")?)?
        }
    };
    let poster = dir.join("poster.png");
    write_atomic(&poster, &poster_png)?;
    report.files.push(poster.clone());
    report.poster = poster;

    // The caption track: SRT and VTT from the same cues, credentials redacted.
    let clean: Vec<Cue> = cues.iter().map(|c| Cue { text: crate::story::redact_url_secrets(&c.text), ..c.clone() }).collect();
    for (ext, text) in [("srt", captions::srt(&clean)), ("vtt", captions::vtt(&clean))] {
        let path = dir.join(format!("{}.{ext}", opts.stem));
        write_atomic(&path, text.as_bytes())?;
        report.files.push(path);
    }

    let script = player.script();
    let html = transcript::transcript_html(
        script.title.as_deref().unwrap_or(""),
        script.summary.as_deref(),
        script.lang.as_deref(),
        chapters,
        &player.element_plans(),
        &clean,
        images,
    );
    let path = dir.join("transcript.html");
    write_atomic(&path, html.as_bytes())?;
    report.files.push(path);

    // post-description.txt (spec D2, R10): every credit line, complete
    // (never `frame_layout::dedupe_sources`'s drop against the tile
    // credit), then the tile credit last -- the social post's own text;
    // it never replaces the visible credit. Omitted when there is
    // nothing to say at all (review round 1).
    let lines = post_description_lines(script, player.tile_credit().as_deref());
    if !lines.is_empty() {
        let path = dir.join("post-description.txt");
        write_atomic(&path, lines.join("\n").as_bytes())?;
        report.files.push(path);
    }

    Ok(report)
}

/// [`write_bundle`]'s `post-description.txt`: the author's own
/// `furniture.sources` groups, every photo's credit (a design card's
/// `photo`, a 1a `image` event's `credit`) and each overlay's
/// `attribution`, each once in first-seen order, then `tile_credit`
/// last, unconditionally -- never swallowed by an earlier, coincidental
/// exact duplicate (review round 1: the tile credit used to share the
/// declared credits' own de-dup set, so a line that happened to repeat
/// it verbatim could make the tile credit never appear at all). Empty
/// with no declared credit and no `tile_credit`, in which case the
/// caller writes no file.
fn post_description_lines(script: &crate::story::script::Script, tile_credit: Option<&str>) -> Vec<String> {
    use crate::story::script::EventAction;
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    let mut push = |s: &str| {
        let s = crate::story::redact_url_secrets(s.trim());
        if !s.is_empty() && seen.insert(s.clone()) {
            lines.push(s);
        }
    };
    for g in script.furniture.sources.iter().flatten() {
        let (key, text) = (g.key.trim(), g.text.trim());
        if !text.is_empty() {
            push(&if key.is_empty() { text.to_string() } else { format!("{key}: {text}") });
        }
    }
    for card in crate::story::design::all_cards(script) {
        if let Some(credit) = card.photo.as_ref().and_then(|p| p.credit.as_deref()) {
            push(credit);
        }
    }
    for e in &script.events {
        if let EventAction::Image(spec) = &e.action {
            if let Some(credit) = spec.credit.as_deref() {
                push(credit);
            }
        }
    }
    for overlay in &script.scene.overlays {
        if let Some(attribution) = overlay.attribution.as_deref() {
            push(attribution);
        }
    }
    if let Some(credit) = tile_credit {
        let credit = crate::story::redact_url_secrets(credit.trim());
        if !credit.is_empty() {
            lines.retain(|l| l != &credit);
            lines.push(credit);
        }
    }
    lines
}

/// `manifest` (a serialised `RenderManifest` written INTO `dir`) with
/// every path inside `dir` rewritten relative to `dir` -- the manifest's
/// own folder -- so the bundle can be moved or published as a folder and
/// its manifest still names its own files (`poster.png`, `<stem>.mp4`, the
/// encoder's output in `sink`, ...). A string is rewritten when, read as a
/// path, it lies under `dir` as given or under `dir`'s canonical form;
/// anything else (an ffmpeg binary, a flag, a URL) is left alone. The
/// recorded `script` is never touched: its paths are the author's.
pub fn relative_to_bundle(manifest: &mut serde_json::Value, dir: &Path) {
    fn walk(v: &mut serde_json::Value, roots: &[PathBuf]) {
        match v {
            serde_json::Value::String(s) => {
                let rel = roots.iter().find_map(|root| Path::new(s.as_str()).strip_prefix(root).ok().map(Path::to_path_buf));
                if let Some(rel) = rel {
                    *s = if rel.as_os_str().is_empty() { ".".to_string() } else { rel.to_string_lossy().into_owned() };
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(|i| walk(i, roots)),
            serde_json::Value::Object(map) => map.values_mut().for_each(|i| walk(i, roots)),
            _ => {}
        }
    }
    let mut roots = vec![dir.to_path_buf()];
    if let Ok(real) = dir.canonicalize() {
        roots.push(real);
    }
    if let serde_json::Value::Object(map) = manifest {
        for (key, value) in map.iter_mut() {
            if key != "script" {
                walk(value, &roots);
            }
        }
    }
}

/// Every string in `v`, rewritten by `f` in place.
fn map_strings(v: &mut serde_json::Value, f: &dyn Fn(&str) -> Option<String>) {
    match v {
        serde_json::Value::String(s) => {
            if let Some(new) = f(s) {
                *s = new;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|i| map_strings(i, f)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|i| map_strings(i, f)),
        _ => {}
    }
}

/// `manifest` as written to a file in `manifest_dir` (controller ruling,
/// Task 17: a manifest never records an absolute local path -- a home
/// directory path is personal data):
///
/// 1. every string passes [`crate::story::redact_url_secrets`] (the
///    script already did; gaps, warnings, credits and chapter text too);
/// 2. every path under `manifest_dir` is relative to it
///    ([`relative_to_bundle`]: `poster.png`, `<stem>.mp4`, ...);
/// 3. every local path under `story_dir` -- whole, or a token of a
///    message -- is relative to the story folder (`img/a.png`);
/// 4. every other absolute local path (`/Volumes/...`, `/tmp/...`, the
///    home directory, a system ffmpeg) is its file name only:
///    `local:<name>` ([`scrub_local_paths`]).
///
/// `--keyless`'s Liberty style is the caller's to name (`keyless:liberty`).
///
/// # Errors
/// The manifest cannot be serialised (it always can).
pub fn manifest_json(manifest: &crate::story::RenderManifest, manifest_dir: &Path, story_dir: &Path) -> anyhow::Result<serde_json::Value> {
    let mut v = serde_json::to_value(manifest)?;
    map_strings(&mut v, &|s| {
        let clean = crate::story::redact_url_secrets(s);
        (clean != s).then_some(clean)
    });
    relative_to_bundle(&mut v, if manifest_dir.as_os_str().is_empty() { Path::new(".") } else { manifest_dir });
    scrub_local_paths(&mut v, story_dir);
    Ok(v)
}

/// Steps 3 and 4 of [`manifest_json`] over every string of `v`.
///
/// Only path values are rewritten -- a whole string that is a local path,
/// or a token of a message delimited by whitespace, a quote, a backtick,
/// a bracket, a comma or a semicolon -- optionally behind `word:` prefixes
/// (`image:`, `pmtiles:`, `source-licence:`). A URL is never touched: its
/// `scheme://` is not a local path, whatever folder names it contains.
///
/// A local path is absolute (`/...` whose first component is a local root
/// such as `Users`, `Volumes`, `tmp`, `private`, `opt`, or exists on this
/// machine; a drive letter; `~/...`). One inside the story folder -- by
/// path components, against the folder's canonical form and its given
/// form when absolute -- becomes relative to it (`img/a.png`); any other
/// becomes `local:<file name>`. A JSON pointer (`/events/3`) is not a
/// local path and is kept.
pub fn scrub_local_paths(v: &mut serde_json::Value, story_dir: &Path) {
    let roots = absolute_forms(story_dir);
    map_strings(v, &|s| {
        let out = scrub_text(s, &roots);
        (out != s).then_some(out)
    });
}

/// `dir`'s absolute forms: canonical (symlinks resolved), and as given
/// when absolute, else made absolute against the current directory.
fn absolute_forms(dir: &Path) -> Vec<PathBuf> {
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let mut forms = Vec::new();
    if let Ok(real) = dir.canonicalize() {
        forms.push(real);
    }
    if let Ok(abs) = std::path::absolute(dir) {
        if !forms.contains(&abs) {
            forms.push(abs);
        }
    }
    forms
}

/// The characters that end a path token inside a message.
fn is_token_break(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';')
}

/// `s` with every local path in it rewritten. A path may hold spaces
/// (`/Users/x/My Stories/a.png`; final review F5): a path token runs on
/// over whitespace to the last later word that still has a `/` in it,
/// stopping at any other break, a word that starts a path or a URL of its
/// own, or the end -- so no folder name is left behind, whole, in the
/// message.
fn scrub_text(s: &str, roots: &[PathBuf]) -> String {
    if let Some(whole) = scrub_token(s, roots) {
        return whole;
    }
    // The message as tokens, each with the break after it (`None` at the end).
    let mut tokens: Vec<(String, Option<char>)> = Vec::new();
    let mut token = String::new();
    for c in s.chars() {
        if is_token_break(c) {
            tokens.push((std::mem::take(&mut token), Some(c)));
        } else {
            token.push(c);
        }
    }
    tokens.push((token, None));
    let starts_own = |t: &str| t.contains("://") || scrub_token(t, roots).is_some();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some((first, _)) = tokens.get(i) {
        let mut end = i;
        if scrub_token(first, roots).is_some() {
            let mut k = i;
            while let (Some((_, Some(brk))), Some((next, _))) = (tokens.get(k), tokens.get(k + 1)) {
                if !brk.is_whitespace() || starts_own(next) {
                    break;
                }
                k += 1;
                if next.contains(['/', '\\']) {
                    end = k;
                }
            }
        }
        let mut joined = String::new();
        for (j, (t, brk)) in tokens.iter().enumerate().take(end + 1).skip(i) {
            joined.push_str(t);
            if j < end {
                joined.extend(*brk);
            }
        }
        out.push_str(&scrub_token(&joined, roots).unwrap_or(joined));
        if let Some((_, Some(brk))) = tokens.get(end) {
            out.push(*brk);
        }
        i = end + 1;
    }
    out
}

/// `token` with its local path rewritten, behind any `word:` prefixes;
/// `None` when it holds no local path.
fn scrub_token(token: &str, roots: &[PathBuf]) -> Option<String> {
    let mut at = 0;
    loop {
        let rest = token.get(at..)?;
        if is_local_absolute(rest) {
            let prefix = token.get(..at)?;
            return Some(format!("{prefix}{}", local_path_name(rest, roots)));
        }
        let colon = rest.find(':')?;
        let (head, after) = (rest.get(..colon)?, rest.get(colon + 1..)?);
        let word = !head.is_empty() && head.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !word || after.starts_with("//") {
            return None;
        }
        at += colon + 1;
    }
}

/// The first components an absolute path on a local disk starts with.
const LOCAL_ROOTS: &[&str] = &[
    "Users", "home", "Volumes", "tmp", "private", "var", "opt", "mnt", "media", "usr", "etc", "Applications", "Library", "System",
    "root", "srv", "nix", "run", "data", "workspace", "cores",
];

fn is_local_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    if b.len() >= 3 && b.first().is_some_and(u8::is_ascii_alphabetic) && b.get(1) == Some(&b':') && matches!(b.get(2), Some(b'/' | b'\\')) {
        return true;
    }
    if path.starts_with("~/") {
        return true;
    }
    let Some(rest) = path.strip_prefix('/') else { return false };
    if rest.starts_with('/') {
        return false;
    }
    let first = rest.split('/').next().unwrap_or("");
    !first.is_empty() && (LOCAL_ROOTS.contains(&first) || Path::new("/").join(first).exists())
}

/// A local path as a manifest records it: relative to the story folder
/// when inside it, else `local:<file name>`.
fn local_path_name(path: &str, roots: &[PathBuf]) -> String {
    let p = Path::new(path);
    let real = crate::story::burn_in::real_location(p);
    for candidate in [p, real.as_path()] {
        for root in roots {
            if let Ok(rel) = candidate.strip_prefix(root) {
                return if rel.as_os_str().is_empty() { ".".to_string() } else { rel.to_string_lossy().into_owned() };
            }
        }
    }
    let name = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("");
    format!("local:{name}")
}

/// [`manifest_json`] written to `path` whole or not at all (a temporary
/// name and a rename): the manifest's folder is `path`'s.
///
/// # Errors
/// See [`manifest_json`]; or the file cannot be written.
pub fn write_manifest(manifest: &crate::story::RenderManifest, path: &Path, story_dir: &Path) -> anyhow::Result<()> {
    write_manifest_value(&manifest_json(manifest, path.parent().unwrap_or(Path::new(".")), story_dir)?, path)
}

/// `value` (a [`manifest_json`] the caller amended) written to `path` as
/// pretty JSON, whole or not at all.
///
/// # Errors
/// The file cannot be written.
pub fn write_manifest_value(value: &serde_json::Value, path: &Path) -> anyhow::Result<()> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)
}

/// `stem` names one file in the bundle folder, never another folder.
fn check_stem(stem: &str) -> anyhow::Result<()> {
    let bad = stem.is_empty()
        || stem == "."
        || stem == ".."
        || stem.contains(['/', '\\', '\0'])
        || Path::new(stem).components().count() != 1;
    anyhow::ensure!(!bad, "the bundle stem `{stem}` must be one file name: no path separator, not `.` or `..`");
    Ok(())
}

/// `frame` as PNG bytes (the `image` crate's encoder: the same bytes for
/// the same pixels, so a still's file hash is stable across runs).
fn encode_png(frame: &Capture) -> anyhow::Result<Vec<u8>> {
    use image::ImageEncoder as _;
    anyhow::ensure!(
        frame.rgba.len() == frame.width as usize * frame.height as usize * 4,
        "frame buffer is not width*height*4 bytes"
    );
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(&frame.rgba, frame.width, frame.height, image::ExtendedColorType::Rgba8)?;
    Ok(out)
}

/// `bytes` at `path`, whole or not at all: a temporary file in the same
/// folder (so the rename never crosses a file system), synced, then
/// renamed over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = parent.join(format!(".{name}.{}.partial", std::process::id()));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::story::player::{RenderOptions, SceneInputs};
    use std::collections::HashMap;
    use std::sync::{Arc, PoisonError};

    /// The GPU tests render labels; hold the lock the other label tests hold.
    fn gpu_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::symbol_pass::LINE_LABELS_REFUSED_TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn inputs(story_dir: &Path) -> SceneInputs {
        let root = env!("CARGO_MANIFEST_DIR");
        let style: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{root}/examples/liberty-style.json")).unwrap()).unwrap();
        let font = std::fs::read(format!("{root}/assets/fonts/NotoSans-Regular.latin-subset.ttf")).unwrap();
        SceneInputs {
            source: Arc::new(crate::story::fixture::FixtureSource),
            source_label: "fixture".into(),
            base_style: style,
            load_style: Box::new(|_| None),
            overlays: HashMap::new(),
            tracks: crate::story::tracks::TrackSet::default(),
            label_fonts: Some((vec![font].into(), "Noto Sans".into())),
            flat_tile_zoom_max: 14,
            raster: None,
            credits: Vec::new(),
            wordmark: None,
            sprites: None,
            sprite_sheet: None,
            sprite_notes: Vec::new(),
            story_dir: story_dir.into(),
            offline: true,
            images_validated: true,
        }
    }

    /// The det fixture story (2 s at 10 fps, 480x270) with two chapters
    /// (`one` 0..1 s, `two` 1..2 s), its one caption (or none), and a
    /// corner image `img/a.png` in a story folder.
    fn det_story(caption: bool) -> (serde_json::Value, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("img")).unwrap();
        image::RgbaImage::from_fn(60, 40, |x, y| image::Rgba([(x * 4) as u8, (y * 6) as u8, 128, 255]))
            .save(dir.path().join("img/a.png"))
            .unwrap();
        let mut events = vec![serde_json::json!({"at": 0.5, "set_layer_visibility": {"layer": "water", "visible": false}})];
        if caption {
            events.push(serde_json::json!({"at": 0.2, "until": 1.8, "caption": {"text": "Marina Bay", "anchor": "bottom-left"}}));
        }
        events.push(serde_json::json!({"at": 0.4, "until": 1.6, "image": {"src": "img/a.png", "alt": "A gradient", "credit": "Photo: Test", "placement": "corner"}}));
        let json = serde_json::json!({"story": 1, "title": "Det", "summary": "A short test story.",
            "output": {"width": 480, "height": 270, "fps": 10},
            "scene": {"style": "examples/liberty-style.json", "basemap": {"source": "fixture"}, "labels": true},
            "camera": [{"type": "jump", "to": {"lon": 103.851, "lat": 1.2905, "zoom": 14.2}},
                       {"type": "fly", "label": "fly", "to": {"lon": 103.856, "lat": 1.2850, "zoom": 15, "pitch": 40}, "duration": 1.0},
                       {"type": "orbit", "degrees": 30, "duration": 1.0}],
            "chapters": [{"id": "one", "title": "Arrival", "from": 0.0, "to": 1.0, "alt": "The camera flies over Marina Bay."},
                         {"id": "two", "title": "Orbit", "from": 1.0, "to": 2.0, "alt": "The camera circles the bay."}],
            "events": events});
        (json, dir)
    }

    fn player(json: &serde_json::Value, story_dir: &Path, cache: &Path) -> Player {
        let raw = serde_json::to_vec(json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        Player::new(&script, &raw, inputs(story_dir), RenderOptions { cache_dir: cache.into(), ..RenderOptions::default() }).unwrap()
    }

    fn bundle(player: &mut Player, dir: &Path) -> BundleReport {
        let (chapters, cues, images) = (player.chapters().to_vec(), player.caption_track(), player.images().to_vec());
        let opts = BundleOptions { dir: dir.into(), stem: "det".into(), video: false, allow_repo_output: false };
        write_bundle(player, &chapters, &cues, &images, &opts).unwrap()
    }

    #[test]
    fn a_bundle_has_every_file_the_spec_lists() {
        let _g = gpu_lock();
        let (json, story) = det_story(true);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        let report = bundle(&mut p, out.path());
        let source = std::fs::read(story.path().join("img/a.png")).unwrap();
        let hash: String = {
            use sha2::Digest as _;
            sha2::Sha256::digest(&source).iter().take(8).map(|b| format!("{b:02x}")).collect()
        };
        let d = out.path();
        let expected = [
            format!("media/{hash}.png"),
            "stills/one.png".into(),
            "stills/two.png".into(),
            "poster.png".into(),
            "det.srt".into(),
            "det.vtt".into(),
            "transcript.html".into(),
            "post-description.txt".into(),
        ];
        let written: Vec<PathBuf> = report.files.iter().map(|f| f.strip_prefix(d).unwrap().to_path_buf()).collect();
        assert_eq!(written, expected.iter().map(PathBuf::from).collect::<Vec<_>>(), "every file, in the documented order");
        for f in &expected {
            assert!(d.join(f).is_file(), "{f}");
        }
        assert_eq!(std::fs::read(d.join(format!("media/{hash}.png"))).unwrap(), source, "the ORIGINAL bytes");
        for still in ["stills/one.png", "stills/two.png", "poster.png"] {
            assert_eq!(image::image_dimensions(d.join(still)).unwrap(), (480, 270), "{still} is the frame size");
        }
        assert_eq!(std::fs::read(d.join("poster.png")).unwrap(), std::fs::read(d.join("stills/one.png")).unwrap(), "poster = first still");
        assert_ne!(std::fs::read(d.join("stills/one.png")).unwrap(), std::fs::read(d.join("stills/two.png")).unwrap());
        let srt = std::fs::read_to_string(d.join("det.srt")).unwrap();
        assert!(srt.starts_with("1\n00:00:00,200 --> 00:00:01,800\nMarina Bay"), "{srt}");
        assert!(std::fs::read_to_string(d.join("det.vtt")).unwrap().contains("Marina Bay"));
        let html = std::fs::read_to_string(d.join("transcript.html")).unwrap();
        assert!(html.contains(&format!("<img src=\"media/{hash}.png\" alt=\"A gradient\"")), "{html}");
        assert!(html.contains("Photo: Test") && html.contains("Marina Bay") && html.contains("A short test story."), "{html}");
        let leftovers: Vec<_> = walk(d).into_iter().filter(|p| p.to_string_lossy().contains("partial")).collect();
        assert!(leftovers.is_empty(), "no temporary file left: {leftovers:?}");
        assert_eq!(report.stills.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), vec!["one", "two"]);
    }

    /// Spec D2, R10's `post_description_lists_every_credit_then_the_tile_credit`:
    /// the author's own `furniture.sources` group, a photo's credit and
    /// the fixture's tile credit, each on its own line, complete (never
    /// dropped against the tile credit the way the colophon row's own
    /// `dedupe_sources` would), the tile credit last.
    #[test]
    fn post_description_lists_every_credit_then_the_tile_credit() {
        let _g = gpu_lock();
        let (mut json, story) = det_story(true);
        json["furniture"] = serde_json::json!({"sources": [{"key": "Data", "text": "Marina Authority"}]});
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        bundle(&mut p, out.path());
        let text = std::fs::read_to_string(out.path().join("post-description.txt")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first(), Some(&"Data: Marina Authority"), "{lines:?}");
        assert!(lines.contains(&"Photo: Test"), "{lines:?}");
        let tile = p.tile_credit().expect("the fixture states a credit");
        assert_eq!(lines.last(), Some(&tile.as_str()), "the tile credit is last: {lines:?}");
        // No duplicate lines, and nothing dropped against the tile credit
        // (D2: this file is complete, unlike the colophon row's own dedupe).
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), lines.len(), "{lines:?}");
    }

    /// A minimal script with no `furniture.sources`, no image or card
    /// photo credit and no overlay attribution -- `post_description_lines`'s
    /// only input besides `tile_credit`.
    fn uncredited_script() -> crate::story::script::Script {
        let v = serde_json::json!({"story": 1, "output": {"width": 480, "height": 270, "fps": 10},
            "scene": {"style": "examples/liberty-style.json", "basemap": {"source": "fixture"}},
            "camera": [{"type": "jump", "to": {"lon": 103.851, "lat": 1.2905, "zoom": 14}}, {"type": "hold", "duration": 1.0}]});
        crate::story::parse(serde_json::to_vec(&v).unwrap().as_slice()).unwrap()
    }

    /// Review round 1: with no declared credit, `post_description_lines`
    /// still holds the tile credit alone.
    #[test]
    fn post_description_holds_only_the_tile_credit_with_no_declared_one() {
        assert_eq!(post_description_lines(&uncredited_script(), Some("Tile Credit Co.")), vec!["Tile Credit Co.".to_string()]);
    }

    /// Review round 1: with neither a declared credit nor a tile credit,
    /// the lines are empty, so [`write_bundle`] writes no file.
    #[test]
    fn post_description_is_empty_with_no_credit_at_all() {
        assert!(post_description_lines(&uncredited_script(), None).is_empty());
    }

    /// Review round 1: a declared credit that happens to repeat the tile
    /// credit word for word is not allowed to swallow it (the old `seen`
    /// set the tile credit shared with the declared ones would drop the
    /// tile credit silently here) -- it still appears, once, last.
    #[test]
    fn post_description_keeps_the_tile_credit_last_even_if_a_source_repeats_it() {
        let mut script = uncredited_script();
        script.furniture.sources = Some(vec![crate::story::script::SourceGroup { key: String::new(), text: "Same Credit".into() }]);
        let lines = post_description_lines(&script, Some("Same Credit"));
        assert_eq!(lines, vec!["Same Credit".to_string()], "{lines:?}");
    }

    /// A [`crate::tile_source::TileSource`] that serves
    /// [`crate::story::fixture::FixtureSource`]'s own tile (so a render
    /// still draws one) but states no attribution -- `FixtureSource`
    /// always does, and nothing else offline-friendly here does not, so
    /// this stands in for "a real source with genuinely no credit"
    /// (review round 1).
    struct UncreditedFixture;

    #[async_trait::async_trait]
    impl crate::tile_source::TileSource for UncreditedFixture {
        async fn fetch(&self, tile: crate::geo::TileId) -> anyhow::Result<Option<Vec<u8>>> {
            crate::story::fixture::FixtureSource.fetch(tile).await
        }
    }

    /// Review round 1: a story with truly nothing to credit writes no
    /// `post-description.txt` at all (an empty file would be worse than
    /// none).
    #[test]
    fn a_story_with_no_credit_at_all_writes_no_post_description() {
        let _g = gpu_lock();
        let (json, story) = det_story(false);
        let mut json = json;
        json["events"] = serde_json::json!([]);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut inputs = inputs(story.path());
        inputs.source = Arc::new(UncreditedFixture);
        inputs.source_label = "uncredited".into();
        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        let mut p = Player::new(&script, &raw, inputs, RenderOptions { cache_dir: cache.path().into(), ..RenderOptions::default() }).unwrap();
        assert!(p.tile_credit().is_none(), "an uncredited source states no tile credit: {:?}", p.tile_credit());
        bundle(&mut p, out.path());
        assert!(!out.path().join("post-description.txt").exists(), "nothing to credit: no file");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    /// Review Focus 5, at the bundle: no captions still gives a valid
    /// empty SRT, a header-only VTT, and a transcript with every chapter,
    /// its still and its alt text.
    #[test]
    fn a_story_with_chapters_and_no_captions_bundles_an_empty_srt_a_webvtt_header_and_every_chapter() {
        let _g = gpu_lock();
        let (json, story) = det_story(false);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        assert!(p.caption_track().is_empty());
        bundle(&mut p, out.path());
        let d = out.path();
        assert_eq!(std::fs::read(d.join("det.srt")).unwrap().len(), 0, "a zero-byte SRT");
        assert_eq!(std::fs::read_to_string(d.join("det.vtt")).unwrap(), "WEBVTT\n\n");
        let html = std::fs::read_to_string(d.join("transcript.html")).unwrap();
        for (id, alt) in [("one", "The camera flies over Marina Bay."), ("two", "The camera circles the bay.")] {
            assert!(html.contains(&format!("<section id=\"{id}\">")), "{id}: {html}");
            assert!(html.contains(&format!("<img src=\"stills/{id}.png\" alt=\"{alt}\">")), "{id}: {html}");
            assert!(d.join(format!("stills/{id}.png")).is_file());
        }
        assert!(!html.contains("class=\"captions\""), "no caption block: {html}");
    }

    /// Fix round 1, finding 4: the manifest written into a bundle names
    /// its files relative to its own folder, whichever form (relative,
    /// absolute) the bundle folder was given in; the script is untouched.
    #[test]
    fn manifest_paths_are_relative_to_the_bundle() {
        let out = tempfile::tempdir().unwrap();
        let dir = out.path().join("b");
        std::fs::create_dir(&dir).unwrap();
        let real = dir.canonicalize().unwrap();
        let mut m = serde_json::json!({
            "poster": dir.join("poster.png"),
            "sink": {"out": real.join("x.mp4"), "command": ["/opt/homebrew/bin/ffmpeg", "-y", dir.join("x.partial.mp4")], "dir": dir},
            "script": {"scene": {"style": dir.join("style.json")}},
            "source": "openfreemap",
        });
        relative_to_bundle(&mut m, &dir);
        assert_eq!(m["poster"], "poster.png");
        assert_eq!(m["sink"]["out"], "x.mp4", "the canonical form too");
        assert_eq!(m["sink"]["command"], serde_json::json!(["/opt/homebrew/bin/ffmpeg", "-y", "x.partial.mp4"]));
        assert_eq!(m["sink"]["dir"], ".");
        assert_eq!(m["script"]["scene"]["style"], serde_json::json!(dir.join("style.json")), "the author's script is kept");
        assert_eq!(m["source"], "openfreemap");
        // A bundle named by a relative path ("bundle/") works the same.
        let mut rel = serde_json::json!({"poster": "bundle/poster.png"});
        relative_to_bundle(&mut rel, Path::new("bundle/"));
        assert_eq!(rel["poster"], "poster.png");
    }

    /// The bundle and its manifest, as the `story` CLI's `--bundle` writes
    /// them: the whole story rendered (a video's manifest), the bundle,
    /// then `det.story.json` with the stills and poster recorded.
    fn bundle_with_manifest(player: &mut Player, dir: &Path, story_dir: &Path) -> (crate::story::RenderManifest, serde_json::Value) {
        let mut m = player.run(&mut crate::story::NullSink, &mut |_| {}).unwrap();
        bundle(player, dir).record_in(&mut m);
        let path = dir.join("det.story.json");
        write_manifest(&m, &path, story_dir).unwrap();
        let written = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        (m, written)
    }

    /// Spec §6 and the controller's privacy ruling: the manifest records
    /// the output settings, every chapter with its still (relative to the
    /// bundle), the coverage counts and the publish table -- and never an
    /// absolute local path.
    #[test]
    fn the_manifest_records_chapters_coverage_format_and_the_publish_table() {
        let _g = gpu_lock();
        let (json, story) = det_story(true);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        let (m, v) = bundle_with_manifest(&mut p, out.path(), story.path());
        assert_eq!(v["format"]["aspect"], "16:9");
        assert!(v["format"]["tier"].is_string() && v["format"]["safe_area"].is_string(), "{}", v["format"]);
        assert_eq!(v["chapters"][0][0]["id"], "one");
        assert_eq!(v["chapters"][0][1], "stills/one.png");
        assert_eq!(v["chapters"][1][1], "stills/two.png");
        assert_eq!(v["chapters"][1][0]["from"], 1.0);
        assert_eq!(v["poster"], "poster.png");
        assert_eq!(v["coverage"]["captions"], 1);
        // Two chapters and one image, each with alt text.
        assert_eq!(v["coverage"]["alt_text"], serde_json::json!([3, 3]));
        assert_eq!(v["coverage"]["gaps"], m.gaps.len());
        let rows = v["publish"].as_array().unwrap();
        let sources: Vec<&str> = rows.iter().map(|r| r["source"].as_str().unwrap()).collect();
        assert_eq!(sources, vec!["basemap", "image:img/a.png"]);
        assert!(rows.iter().all(|r| r["verdict"] == "ok"), "{rows:?}");
        assert_eq!(rows[1]["credit"], "Photo: Test");
        assert_eq!(v["publishable"], true);
        assert_eq!(m.publishable, crate::story::burn_in::publishable(&m.publish));
        assert_no_local_path(&out.path().join("det.story.json"), &[story.path(), out.path()]);
    }

    /// No byte of `manifest` names the home directory, `/Users/`,
    /// `/home/`, or any of `dirs` (as given or canonical).
    fn assert_no_local_path(manifest: &Path, dirs: &[&Path]) {
        let text = std::fs::read_to_string(manifest).unwrap();
        let mut banned: Vec<String> = vec!["/Users/".into(), "/home/".into()];
        if let Some(home) = std::env::var_os("HOME").filter(|h| h.len() > 1) {
            banned.push(home.to_string_lossy().into_owned());
        }
        for d in dirs {
            banned.push(d.to_string_lossy().into_owned());
            banned.push(d.canonicalize().unwrap().to_string_lossy().into_owned());
        }
        for b in &banned {
            assert!(!text.contains(b.as_str()), "the manifest records `{b}`:\n{text}");
        }
        fn strings<'v>(v: &'v serde_json::Value, out: &mut Vec<&'v str>) {
            match v {
                serde_json::Value::String(s) => out.push(s),
                serde_json::Value::Array(a) => a.iter().for_each(|i| strings(i, out)),
                serde_json::Value::Object(m) => m.values().for_each(|i| strings(i, out)),
                _ => {}
            }
        }
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut all = Vec::new();
        strings(&value, &mut all);
        // Every string that is a local absolute path (a JSON pointer such
        // as `/events/2` is not one).
        let absolute: Vec<&&str> = all.iter().filter(|s| s.starts_with('~') || is_local_absolute(s)).collect();
        assert!(absolute.is_empty(), "absolute paths in the manifest: {absolute:?}");
    }

    /// F33 (Review Focus 5, at the manifest): chapters and no captions is
    /// a caption count of 0, no error, and every chapter still recorded.
    #[test]
    fn a_story_with_chapters_and_no_captions_has_a_caption_count_of_0() {
        let _g = gpu_lock();
        let (json, story) = det_story(false);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        let (m, v) = bundle_with_manifest(&mut p, out.path(), story.path());
        assert_eq!(m.coverage.captions, 0);
        assert_eq!(v["coverage"]["captions"], 0);
        assert_eq!(v["chapters"].as_array().unwrap().len(), 2);
        assert!(v["chapters"].as_array().unwrap().iter().all(|c| c[1].is_string()), "{}", v["chapters"]);
    }

    /// R7 through a real render: an image without a credit is drawn, its
    /// row is blocked, and the manifest is not publishable.
    #[test]
    fn an_uncredited_image_makes_the_manifest_unpublishable() {
        let _g = gpu_lock();
        let (mut json, story) = det_story(false);
        let image = json["events"].as_array_mut().unwrap().iter_mut().find(|e| e.get("image").is_some()).unwrap();
        image["image"].as_object_mut().unwrap().remove("credit");
        let cache = tempfile::tempdir().unwrap();
        let mut p = player(&json, story.path(), cache.path());
        let m = p.run(&mut crate::story::NullSink, &mut |_| {}).unwrap();
        let row = m.publish.iter().find(|r| r.source == "image:img/a.png").unwrap();
        assert_eq!(row.verdict, crate::story::burn_in::RowVerdict::Blocked);
        assert!(!m.publishable);
        assert_eq!(m.publishable_reason.as_deref(), Some("image:img/a.png: image img/a.png has no credit"));
    }

    /// Spec §8.6 at the bundle: credentials in every URL form -- a basemap
    /// `access_token` (served by a local tile server), an image `key=`
    /// (served from the image cache, as an earlier validation leaves it), a
    /// `user:pass@` link in an overlay's attribution, and a `token=` in a
    /// caption -- reach no file of the bundle or its manifest (text files
    /// and PNG chunks alike: every byte of every file is searched).
    #[test]
    fn no_output_file_contains_a_secret() {
        let _g = gpu_lock();
        let server_rt = tokio::runtime::Runtime::new().unwrap();
        let server = server_rt.block_on(wiremock::MockServer::start());
        server_rt.block_on(
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(crate::story::fixture::FIXTURE_TILE))
                .mount(&server),
        );
        let (mut json, story) = det_story(true);
        std::fs::copy(format!("{}/examples/liberty-style.json", env!("CARGO_MANIFEST_DIR")), story.path().join("liberty.json")).unwrap();
        json["scene"]["style"] = "liberty.json".into();
        json["scene"]["basemap"] = serde_json::json!({
            "tiles": [format!("{}/{{z}}/{{x}}/{{y}}.mvt?access_token=SECRET1", server.uri())],
            "max_zoom": 14,
            "attribution": "© Test tiles"
        });
        json["scene"]["overlays"] = serde_json::json!([{
            "id": "route",
            "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
                "geometry": {"type": "LineString", "coordinates": [[103.850, 1.288], [103.856, 1.286]]}}]},
            "layers": [{"id": "route-line", "type": "line", "paint": {"line-color": "#d33", "line-width": 4}}],
            "attribution": "Route data: https://user:SECRET3@data.example/route"
        }]);
        let src = "https://images.example/a.png?key=SECRET2";
        for e in json["events"].as_array_mut().unwrap() {
            if let Some(image) = e.get_mut("image") {
                image["src"] = src.into();
            }
            if let Some(caption) = e.get_mut("caption") {
                caption["text"] = "Marina Bay token=SECRET4".into();
            }
        }
        let image_cache = tempfile::tempdir().unwrap();
        let key: String = {
            use sha2::Digest as _;
            sha2::Sha256::digest(src.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
        };
        let png = std::fs::read(story.path().join("img/a.png")).unwrap();
        crate::image_cache::write(image_cache.path(), &key, &png, u64::MAX).unwrap();

        let raw = serde_json::to_vec(&json).unwrap();
        let script = crate::story::parse(&raw).unwrap();
        let inputs = crate::story::scene_inputs(&script, story.path(), true).unwrap();
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let opts = RenderOptions {
            cache_dir: cache.path().into(),
            image_cache_dir: Some(image_cache.path().into()),
            tile_timeout: std::time::Duration::from_secs(5),
            ..RenderOptions::default()
        };
        let mut p = Player::new(&script, &raw, inputs, opts).unwrap();
        assert_eq!(p.images().len(), 1, "the keyed image loaded from the cache");
        let (m, v) = bundle_with_manifest(&mut p, out.path(), story.path());
        assert!(m.attribution.contains("Route data:"), "the overlay is credited: {}", m.attribution);
        let route = m.publish.iter().find(|r| r.source == "overlay:route").unwrap();
        assert!(route.credit.as_deref().is_some_and(|c| c.contains("[REDACTED]")), "{route:?}");
        assert!(v["publish"].as_array().unwrap().iter().any(|r| r["source"].as_str().is_some_and(|s| s.starts_with("image:https://images.example/a.png"))));
        let mut searched = 0;
        for f in walk(out.path()) {
            assert!(!f.to_string_lossy().contains("SECRET"), "{}", f.display());
            let bytes = std::fs::read(&f).unwrap();
            assert!(!bytes.windows(6).any(|w| w == b"SECRET"), "{} holds a credential", f.display());
            searched += 1;
        }
        assert!(searched >= 8, "manifest, media, 2 stills, poster, srt, vtt, transcript: {searched}");
        assert_no_local_path(&out.path().join("det.story.json"), &[story.path(), out.path()]);
    }

    /// The privacy ruling (Task 17 fix round 1): a path inside the story
    /// folder is relative to it, whole or inside a message; every other
    /// absolute local path -- the home directory, a `/Volumes` drive, a
    /// system ffmpeg -- is its file name only; JSON pointers and URLs are
    /// not paths and are kept.
    #[test]
    fn local_paths_are_story_relative_or_their_file_name_only() {
        let root = tempfile::tempdir().unwrap();
        let story = root.path().join("stories/tokyo");
        std::fs::create_dir_all(&story).unwrap();
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/home/someone"), PathBuf::from);
        let mut v = serde_json::json!({
            "script": {"scene": {"style": story.join("style.json")}, "src": "/Volumes/Drive/photos/a.png"},
            "gaps": [format!("image {} (file_not_found)", story.join("img/a.png").display()), "event /events/3 at 12 s is after the last frame"],
            "sink": {"ffmpeg": home.join("bin/ffmpeg"), "system": "/opt/homebrew/bin/ffmpeg", "command": ["/opt/homebrew/bin/ffmpeg", "-y"]},
            "publish": [{"source": "image:/Volumes/Drive/photos/b.png"}, {"source": "basemap", "reason": "add output.acknowledge: [\"source-licence:pmtiles:/tmp/x/city.pmtiles\"]"}],
            "dir": story,
            "canonical": story.canonicalize().unwrap().join("data/route.json"),
            "pointer": "/scene/overlays/0",
            "url": "https://cdn.example.com/tmp/Users/a.png",
            "source": "openfreemap",
        });
        scrub_local_paths(&mut v, &story);
        assert_eq!(v["script"]["scene"]["style"], "style.json");
        assert_eq!(v["script"]["src"], "local:a.png", "a /Volumes image src is its name only");
        assert_eq!(v["gaps"][0], "image img/a.png (file_not_found)");
        assert_eq!(v["gaps"][1], "event /events/3 at 12 s is after the last frame");
        assert_eq!(v["sink"]["ffmpeg"], "local:ffmpeg");
        assert_eq!(v["sink"]["system"], "local:ffmpeg", "no absolute path, even a system one");
        assert_eq!(v["sink"]["command"], serde_json::json!(["local:ffmpeg", "-y"]));
        assert_eq!(v["publish"][0]["source"], "image:local:b.png");
        assert_eq!(v["publish"][1]["reason"], "add output.acknowledge: [\"source-licence:pmtiles:local:city.pmtiles\"]");
        assert_eq!(v["dir"], ".");
        assert_eq!(v["canonical"], "data/route.json");
        assert_eq!(v["pointer"], "/scene/overlays/0");
        assert_eq!(v["url"], "https://cdn.example.com/tmp/Users/a.png");
        assert_eq!(v["source"], "openfreemap");
        assert!(!v.to_string().contains(&*home.to_string_lossy()), "{v}");
    }

    /// Final review F5: a path with spaces in it is scrubbed whole -- an
    /// outside one to its whole file name with no folder part, an inside
    /// one relative to the story folder -- in a message, in backticks or
    /// alone; the words around it, and a second path, are kept apart.
    #[test]
    fn a_path_with_spaces_is_scrubbed_whole() {
        let root = tempfile::tempdir().unwrap();
        let story = root.path().join("My Stories/tokyo");
        std::fs::create_dir_all(story.join("img")).unwrap();
        let inside = story.join("img/the harbour.png");
        let mut v = serde_json::json!({
            "alone": "/Users/x/My Stories/a.png",
            "message": "cannot read /Users/x/My Stories/a.png: no such file",
            "quoted": "cannot read `/Users/x/My Stories/a.png` (file_not_found)",
            "spaced_name": "cannot read /Users/x/My Stories/a b.png",
            "two": "copied /Users/x/My Stories/a.png to /Volumes/Drive/Old Photos/b.png",
            "inside": format!("image {} (file_not_found)", inside.display()),
        });
        scrub_local_paths(&mut v, &story);
        assert_eq!(v["alone"], "local:a.png");
        assert_eq!(v["message"], "cannot read local:a.png: no such file");
        assert_eq!(v["quoted"], "cannot read `local:a.png` (file_not_found)");
        assert_eq!(v["spaced_name"], "cannot read local:a b.png");
        assert_eq!(v["two"], "copied local:a.png to local:b.png");
        assert_eq!(v["inside"], "image img/the harbour.png (file_not_found)");
        let text = v.to_string();
        for leaked in ["My Stories", "Users", "Old Photos", "Drive"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }

    /// Review finding 3(b): a RELATIVE story folder (as the agent may pass
    /// it) never rewrites a URL that happens to contain its name.
    #[test]
    fn a_relative_story_dir_leaves_a_url_with_its_name_intact() {
        let mut v = serde_json::json!({
            "script": {"events": [{"image": {"src": "https://cdn.example.com/stories/a.png"}}]},
            "publish": [{"source": "image:https://cdn.example.com/stories/a.png"}],
            "text": "see stories/a.png and https://x.example/stories/",
        });
        let before = v.clone();
        scrub_local_paths(&mut v, Path::new("stories"));
        assert_eq!(v, before);
    }

    #[test]
    fn a_still_is_the_frame_at_its_time() {
        let _g = gpu_lock();
        let (json, story) = det_story(true);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        let report = bundle(&mut p, out.path());
        let chapters = p.chapters().to_vec();
        for (id, path) in &report.stills {
            let chapter = chapters.iter().find(|c| c.id == *id).unwrap();
            let decoded = image::open(path).unwrap().to_rgba8();
            let separately = p.render_still_at(chapter.still_t).unwrap();
            let file = Capture { width: decoded.width(), height: decoded.height(), rgba: decoded.into_raw(), ..separately.clone() };
            assert_eq!(file.sha256_hex(), separately.sha256_hex(), "chapter {id}: the file is the frame at {} s", chapter.still_t);
        }
        // A fresh player renders the same still, bit for bit.
        let cache2 = tempfile::tempdir().unwrap();
        let mut fresh = player(&json, story.path(), cache2.path());
        let one = &chapters[0];
        assert_eq!(fresh.render_still_at(one.still_t).unwrap().sha256_hex(), p.render_still_at(one.still_t).unwrap().sha256_hex());
        assert_eq!(p.still_frame(one.still_t), 5, "the midpoint of 0..1 s at 10 fps");
    }

    #[test]
    fn poster_at_renders_its_own_frame() {
        let _g = gpu_lock();
        let (mut json, story) = det_story(false);
        json["output"]["poster_at"] = 1.9.into();
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        bundle(&mut p, out.path());
        let d = out.path();
        let poster = std::fs::read(d.join("poster.png")).unwrap();
        assert_ne!(poster, std::fs::read(d.join("stills/one.png")).unwrap());
        let decoded = image::load_from_memory(&poster).unwrap().to_rgba8();
        assert_eq!(decoded.into_raw(), p.render_still_at(1.9).unwrap().rgba, "the frame at poster_at");
    }

    #[test]
    fn a_hostile_name_never_leaves_the_bundle_and_a_bad_stem_or_repo_dir_is_refused() {
        let _g = gpu_lock();
        let (json, story) = det_story(false);
        let (cache, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut p = player(&json, story.path(), cache.path());
        let chapters = p.chapters().to_vec();
        let mut images = p.images().to_vec();
        images[0].bundle_name = "../../escaped.png".into();
        images[0].src = "https://user:pw@example.com/../../a.png?key=SECRET".into();
        let opts = |stem: &str, dir: &Path| BundleOptions { dir: dir.into(), stem: stem.into(), video: false, allow_repo_output: false };
        for stem in ["", ".", "..", "a/b", "a\\b"] {
            let e = write_bundle(&mut p, &chapters, &[], &images, &opts(stem, out.path())).unwrap_err();
            assert!(format!("{e:#}").contains("one file name"), "{stem}: {e:#}");
        }
        let inside_repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target-bundle-refusal-test");
        let e = write_bundle(&mut p, &chapters, &[], &images, &opts("det", &inside_repo)).unwrap_err();
        assert!(format!("{e:#}").contains("--allow-repo-output") && !inside_repo.exists(), "{e:#}");
        let report = write_bundle(&mut p, &chapters, &[], &images, &opts("det", out.path())).unwrap();
        let (src, copy) = &report.media[0];
        assert!(copy.starts_with(out.path().join("media")) && copy.extension().unwrap() == "png", "{copy:?}");
        assert!(!src.contains("SECRET") && !src.contains("pw@"), "{src}");
        assert!(!out.path().parent().unwrap().join("escaped.png").exists());
        let html = std::fs::read_to_string(out.path().join("transcript.html")).unwrap();
        assert!(!html.contains("SECRET") && !html.contains("escaped"), "{html}");
    }
}

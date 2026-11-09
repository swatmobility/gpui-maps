//! Spec §8.5: a story end to end through the user's ffmpeg -- a real
//! H.264 MP4 checked with ffprobe and by its pixels -- plus the no-ffmpeg
//! fallback (PNGs and `encode.sh`) and an ffmpeg that dies early.
//!
//! Needs ffmpeg for the first test: without one it prints `SKIPPED` and
//! passes. Set `GPUI_MAP_FFMPEG` to name a binary off `PATH`.
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_maps::capture::Capture;
use gpui_maps::story::sink::{self, Codec, FfmpegChoice, FfmpegInfo, FfmpegSink};
use gpui_maps::story::{FrameSink, RenderManifest};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn ffmpeg() -> Option<FfmpegInfo> {
    let f = sink::detect_ffmpeg();
    if f.is_none() {
        eprintln!("SKIPPED: no ffmpeg on PATH (set GPUI_MAP_FFMPEG)");
    }
    f
}

/// `~/.cache/gpui-map-stories/<name>`, emptied and made fresh.
fn dirs_out(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
    let dir = home.join(".cache/gpui-map-stories").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn inputs() -> gpui_maps::story::SceneInputs {
    let style: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{ROOT}/examples/liberty-style.json")).unwrap()).unwrap();
    let font = std::fs::read(format!("{ROOT}/assets/fonts/NotoSans-Regular.latin-subset.ttf")).unwrap();
    gpui_maps::story::SceneInputs {
        source: Arc::new(gpui_maps::story::fixture::FixtureSource),
        source_label: "fixture".into(),
        base_style: style,
        load_style: Box::new(|_| None),
        overlays: HashMap::new(),
        tracks: gpui_maps::story::tracks::TrackSet::default(),
        label_fonts: Some((vec![font].into(), "Noto Sans".into())),
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
    }
}

/// Render the fixture story at `rel` into `mp4` through `ff`.
fn render_fixture_story(rel: &str, mp4: &Path, ff: &FfmpegInfo) -> RenderManifest {
    let raw = std::fs::read(format!("{ROOT}/{rel}")).unwrap();
    let script = gpui_maps::story::parse(&raw).unwrap();
    let out = script.output();
    let mut sink = FfmpegSink::start(ff, mp4, out.width, out.height, out.fps, Codec::H264).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let opts = gpui_maps::story::RenderOptions { cache_dir: cache.path().into(), ..Default::default() };
    let manifest = gpui_maps::story::render(&script, &raw, inputs(), opts, &mut sink, &mut |_| {}).unwrap();
    std::fs::write(mp4.with_extension("story.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    manifest
}

/// The style's `background` layer colour: a literal `#rrggbb` in
/// liberty, so the same at z14 as at every zoom.
fn background_colour() -> [u8; 3] {
    let style: serde_json::Value = serde_json::from_slice(&std::fs::read(format!("{ROOT}/examples/liberty-style.json")).unwrap()).unwrap();
    let layer = style["layers"].as_array().unwrap().iter().find(|l| l["type"] == "background").unwrap();
    let hex = layer["paint"]["background-color"].as_str().expect("liberty's background is a literal colour");
    let hex = hex.strip_prefix('#').unwrap();
    assert_eq!(hex.len(), 6, "background colour {hex}");
    let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
    [c(0), c(2), c(4)]
}

#[test]
fn a_three_second_1080p30_story_encodes_to_a_real_h264_mp4() {
    let Some(ff) = ffmpeg() else { return };
    let out_dir = dirs_out("e2e");
    let mp4 = out_dir.join("e2e.mp4");
    let manifest = render_fixture_story("tests/fixtures/stories/e2e-3s.json", &mp4, &ff);
    assert_eq!(manifest.frames.len(), 90);
    assert!(manifest.frames.iter().all(|r| r.tiles_drawn > 0));
    // `tiles_missing` is what proves nothing is unpainted. FixtureSource
    // serves every tile, so this guards the reporting path, not tile
    // availability; the real-source case is the Task 17/18 runs.
    let missing: Vec<_> = manifest.frames.iter().filter(|r| !r.tiles_missing.is_empty()).map(|r| (r.n, &r.tiles_missing)).take(3).collect();
    assert!(missing.is_empty(), "frames with missing tiles: {missing:?}");
    // The manifest names the ffmpeg and the exact command (spec §5.3).
    let sink_report = &manifest.sink;
    assert_eq!(sink_report["kind"], "ffmpeg");
    // By file name and version only: never the binary's path (Task 17).
    assert_eq!(sink_report["ffmpeg"]["name"].as_str(), Some(ff.name().as_str()));
    assert!(sink_report["ffmpeg"].get("path").is_none(), "{}", sink_report["ffmpeg"]);
    assert_eq!(sink_report["ffmpeg"]["version"].as_str(), Some(ff.version.as_str()));
    let command: Vec<&str> = sink_report["command"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(command.windows(2).any(|w| w == ["-color_primaries", "bt709"]), "{command:?}");
    assert_eq!(command.first().copied(), Some(ff.name().as_str()), "argv[0] by name");
    // `command` is the argv that ran, into the partial file.
    assert_eq!(command.last().copied(), sink_report["written_as"].as_str());
    assert!(command.last().unwrap().ends_with("e2e.partial.mp4"), "{command:?}");
    assert!(!mp4.with_file_name("e2e.partial.mp4").exists(), "the partial file is renamed on success");
    // ffprobe: 90 frames, 1920x1080, yuv420p, h264, bt709, 3.000 s
    let probe = std::process::Command::new(ff.path.with_file_name("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames", "-show_entries", "stream=codec_name,width,height,pix_fmt,color_space,nb_read_frames:format=duration", "-of", "json"])
        .arg(&mp4)
        .output()
        .unwrap();
    let j: serde_json::Value = serde_json::from_slice(&probe.stdout).unwrap();
    let s = &j["streams"][0];
    assert_eq!(
        (s["codec_name"].as_str(), s["width"].as_u64(), s["height"].as_u64(), s["pix_fmt"].as_str(), s["color_space"].as_str(), s["nb_read_frames"].as_str()),
        (Some("h264"), Some(1920), Some(1080), Some("yuv420p"), Some("bt709"), Some("90")),
        "{j}"
    );
    // All four colour tags, not only the matrix the brief's probe reads:
    // ffmpeg 8 drops -color_primaries/-color_trc without the setparams.
    let tags = std::process::Command::new(ff.path.with_file_name("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=color_range,color_space,color_transfer,color_primaries", "-of", "json"])
        .arg(&mp4)
        .output()
        .unwrap();
    let t: serde_json::Value = serde_json::from_slice(&tags.stdout).unwrap();
    let t = &t["streams"][0];
    assert_eq!(
        (t["color_range"].as_str(), t["color_space"].as_str(), t["color_transfer"].as_str(), t["color_primaries"].as_str()),
        (Some("tv"), Some("bt709"), Some("bt709"), Some("bt709")),
        "{t}"
    );
    assert!((j["format"]["duration"].as_str().unwrap().parse::<f64>().unwrap() - 3.0).abs() < 0.01, "{j}");
    // Extract frames 0, 45, 89 and check they are not grey
    let mut frames = Vec::new();
    for n in [0, 45, 89] {
        let png = out_dir.join(format!("f{n}.png"));
        let st = std::process::Command::new(&ff.path)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(&mp4)
            .args(["-vf", &format!("select=eq(n\\,{n})"), "-vframes", "1"])
            .arg(&png)
            .status()
            .unwrap();
        assert!(st.success());
        println!("EXTRACTED {}", png.display());
        frames.push(image::open(&png).unwrap().to_rgba8());
    }
    let bg = background_colour();
    for (i, f) in frames.iter().enumerate() {
        assert_not_grey(f, i);
        assert_not_background(f, i, bg);
    }
    let differing = frames[0].pixels().zip(frames[1].pixels()).filter(|(a, b)| (0..3).any(|c| a[c].abs_diff(b[c]) > 8)).count();
    assert!(differing as f64 > 0.05 * f64::from(1920 * 1080), "frame 45 must differ from frame 0: {differing}");
}

/// Spec §8.5 step 4: fewer than 2% of pixels within ±6 of mid-grey and
/// more than 200 distinct colours; for detail, the busy-block share
/// (Ruling C3) replaces the luminance sd: 32×32 blocks of bt709 luminance
/// (partial edge blocks dropped, every 2nd pixel sampled), a block busy
/// when its range is above 8, and at least 10% of blocks busy. A flat
/// frame scores 0; measured 0.428 / 0.291 / 0.385 on frames 0 / 45 / 89.
fn assert_not_grey(img: &image::RgbaImage, which: usize) {
    let n = f64::from(img.width() * img.height());
    let near = |p: &image::Rgba<u8>, c: [u8; 3]| (0..3).all(|i| p[i].abs_diff(c[i]) <= 6);
    let grey = img.pixels().filter(|p| near(p, [128, 128, 128])).count() as f64 / n;
    let busy = busy_block_share(img);
    let distinct = img.pixels().map(|p| (p[0], p[1], p[2])).collect::<std::collections::HashSet<_>>().len();
    println!("frame #{which}: grey {grey:.4} busy {busy:.3} distinct {distinct}");
    assert!(grey < 0.02 && busy >= 0.10 && distinct > 200, "frame #{which}: grey {grey:.3} busy {busy:.3} distinct {distinct}");
}

/// The share of 32×32 blocks whose bt709 luminance range is above 8.
fn busy_block_share(img: &image::RgbaImage) -> f64 {
    const B: u32 = 32;
    let luma = |x: u32, y: u32| {
        let p = img.get_pixel(x, y);
        0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2])
    };
    let (bw, bh) = (img.width() / B, img.height() / B);
    let mut busy = 0usize;
    for by in 0..bh {
        for bx in 0..bw {
            let (mut lo, mut hi) = (f64::MAX, f64::MIN);
            for y in (by * B..(by + 1) * B).step_by(2) {
                for x in (bx * B..(bx + 1) * B).step_by(2) {
                    let l = luma(x, y);
                    lo = lo.min(l);
                    hi = hi.max(l);
                }
            }
            if hi - lo > 8.0 {
                busy += 1;
            }
        }
    }
    busy as f64 / f64::from(bw * bh)
}

/// The frame differs from an all-background frame by more than 5% of its
/// pixels (a pixel is background within ±8 on every channel: the default
/// decode reads 2-4 levels dark). The spec's "<2% background" is replaced
/// by this (Ruling C2: liberty draws bare land, and here the hidden water,
/// in the background colour).
fn assert_not_background(img: &image::RgbaImage, which: usize, bg: [u8; 3]) {
    let n = f64::from(img.width() * img.height());
    let background = img.pixels().filter(|p| (0..3).all(|i| p[i].abs_diff(bg[i]) <= 8)).count() as f64;
    let bg_share = background / n;
    assert!(n - background > 0.05 * n, "frame #{which} is {bg_share:.3} style background (an all-background frame)");
}

/// A `w`×`h` frame of a flat colour.
fn flat_frame(w: u32, h: u32, rgba: [u8; 4]) -> Capture {
    Capture {
        width: w,
        height: h,
        rgba: rgba.iter().copied().cycle().take((w * h * 4) as usize).collect(),
        tiles_drawn: 1,
        label_occlusion: (0, 0.0),
        mask_ring_tiles: 0,
        tiles_missing: Vec::new(),
        vertices: 0,
        triangles: 0,
        transition: 0.0,
        raster_tiles_drawn: 0,
        labels_drawn: 0,
        donuts_drawn: 0,
        donuts_truncated: 0,
        overlay_labels_drawn: 0,
        altitude_points_drawn: 0,
        icons_drawn: 0,
        line_glyphs_drawn: 0,
        line_glyphs_dropped: 0,
        story_retessellations: 0,
        story_reshapes: 0,
        chrome_layout_us: 0.0,
        layout_warnings: Vec::new(),
    }
}

#[test]
fn a_missing_ffmpeg_writes_the_sequence_and_encode_sh() {
    let dir = tempfile::tempdir().unwrap();
    let choice = FfmpegChoice::Path("/nonexistent/ffmpeg".into());
    assert!(sink::detect_ffmpeg_with(&choice).is_none());
    let mut s = sink::video_sink(&choice, &dir.path().join("harbour-run.mp4"), 1920, 1080, 30, Codec::H264).unwrap();
    for n in 0..3 {
        s.push(n, &flat_frame(1920, 1080, [40, 90, 160, 255])).unwrap();
    }
    let report = s.finish().unwrap();
    assert_eq!(report["kind"], "png_sequence_encode_later");
    for n in 0..3 {
        let png = dir.path().join(format!("harbour-run-frames/{n:06}.png"));
        assert_eq!(image::image_dimensions(&png).unwrap(), (1920, 1080), "{}", png.display());
    }
    assert!(!dir.path().join("harbour-run.mp4").exists());
    let sh = std::fs::read_to_string(dir.path().join("harbour-run-encode.sh")).unwrap();
    // The output name the caller asked for, not a fixed `story.mp4`.
    assert!(sh.lines().last().unwrap().ends_with(" harbour-run.mp4"), "{sh}");
    assert!(sh.contains("-framerate 30 -i harbour-run-frames/%06d.png"), "{sh}");
    for needed in ["-c:v libx264 -preset slow -crf 18", "-colorspace bt709", "-color_range tv", "-movflags +faststart", "format=yuv420p"] {
        assert!(sh.contains(needed), "encode.sh lacks {needed}:\n{sh}");
    }
    assert!(report["command"].as_str().unwrap().contains("-framerate 30"));
}

/// F2: two renders into one directory keep their own frames and scripts,
/// a re-render of the same output drops the stale frames of a longer
/// earlier run, and `encode.sh` follows the codec asked for.
#[test]
fn fallback_renders_into_one_directory_do_not_share_frames() {
    let dir = tempfile::tempdir().unwrap();
    let choice = FfmpegChoice::Path("/nonexistent/ffmpeg".into());
    let frame = flat_frame(64, 36, [40, 90, 160, 255]);
    let run = |name: &str, frames: usize, codec: Codec| {
        let mut s = sink::video_sink(&choice, &dir.path().join(name), 64, 36, 30, codec).unwrap();
        for n in 0..frames {
            s.push(n, &frame).unwrap();
        }
        s.finish().unwrap();
    };
    run("a.mp4", 5, Codec::H264);
    run("b.webm", 2, Codec::Vp9);
    let count = |d: &str| std::fs::read_dir(dir.path().join(d)).unwrap().count();
    assert_eq!(count("a-frames"), 5);
    assert_eq!(count("b-frames"), 2);
    let vp9 = std::fs::read_to_string(dir.path().join("b-encode.sh")).unwrap();
    assert!(vp9.contains("libvpx-vp9") && vp9.contains("-f webm") && vp9.trim_end().ends_with(" b.webm"), "{vp9}");
    assert!(!vp9.contains("libx264"), "{vp9}");
    // A shorter re-render of `a` leaves no frame 3 or 4 behind.
    run("a.mp4", 2, Codec::H264);
    assert_eq!(count("a-frames"), 2);
    assert!(std::fs::read_to_string(dir.path().join("a-encode.sh")).unwrap().contains("libx264"));
}

/// A fake ffmpeg at `dir/ffmpeg` that answers the probes, then on a real
/// run reads 1 byte, writes a few bytes to its output and exits 1 with a
/// message on stderr.
#[cfg(unix)]
fn fake_ffmpeg(dir: &Path) -> PathBuf {
    fake_ffmpeg_running(
        dir,
        r#"head -c 1 >/dev/null
printf 'not a video' > "$last"
echo "fake-ffmpeg: the encoder refused its input" >&2
exit 1
"#,
    )
}

/// A fake ffmpeg that answers the probes and then runs `body` (with
/// `$last` set to its output path) on a real run.
#[cfg(unix)]
fn fake_ffmpeg_running(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("ffmpeg");
    std::fs::write(
        &path,
        r#"#!/bin/sh
case "$1" in
  -version) echo "ffmpeg version fake-1"; exit 0 ;;
esac
if [ "$1" = "-hide_banner" ] && [ "$2" = "-encoders" ]; then
  printf 'Encoders:\n ------\n V....D libx264              fake H.264\n'
  exit 0
fi
for a in "$@"; do last="$a"; done
"#
        .to_string()
            + body,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn an_ffmpeg_that_exits_early_is_an_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let info = sink::detect_ffmpeg_with(&FfmpegChoice::Path(fake_ffmpeg(dir.path()))).unwrap();
    assert_eq!(info.version, "ffmpeg version fake-1");
    assert!(info.has("libx264"));
    let out = dir.path().join("out.mp4");
    let started = Instant::now();
    let mut s = FfmpegSink::start(&info, &out, 256, 256, 30, Codec::H264).unwrap();
    let frame = flat_frame(256, 256, [10, 20, 30, 255]);
    let mut err = None;
    for n in 0..10_000 {
        if let Err(e) = s.push(n, &frame) {
            err = Some(e);
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "still pushing after 10 s");
    }
    let err = match err {
        Some(e) => e,
        None => s.finish().unwrap_err(),
    };
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    let msg = format!("{err:#}");
    assert!(msg.contains("fake-ffmpeg: the encoder refused its input"), "{msg}");
    // The player aborts the sink after an error; a failed encode keeps its
    // partial file for diagnosis, and never takes the final name.
    s.abort();
    assert!(dir.path().join("out.partial.mp4").exists(), "{msg}");
    assert!(!out.exists());
}

/// `pid` is gone: not signallable, and not a zombie in `ps`.
#[cfg(unix)]
fn assert_reaped(pid: u32) {
    let alive = std::process::Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().unwrap();
    assert!(!alive.success(), "ffmpeg {pid} still accepts signals");
    let ps = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
    let stat = String::from_utf8_lossy(&ps.stdout);
    assert!(stat.trim().is_empty(), "ffmpeg {pid} is still in the process table: {stat:?}");
}

#[cfg(unix)]
#[test]
fn an_aborted_encode_kills_ffmpeg_and_leaves_no_video() {
    let Some(ff) = ffmpeg() else { return };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("cancelled.mp4");
    let mut s = FfmpegSink::start(&ff, &out, 256, 256, 30, Codec::H264).unwrap();
    for n in 0..5 {
        s.push(n, &flat_frame(256, 256, [200, 100, 50, 255])).unwrap();
    }
    let pid = s.pid().unwrap();
    s.abort();
    assert!(s.pid().is_none());
    assert_reaped(pid);
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "a cancel leaves nothing behind: {left:?}");
}

#[cfg(unix)]
#[test]
fn an_ffmpeg_that_stops_reading_is_killed_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake_ffmpeg_running(dir.path(), "printf 'partial' > \"$last\"\nexec sleep 3600\n");
    let info = sink::detect_ffmpeg_with(&FfmpegChoice::Path(fake)).unwrap();
    let out = dir.path().join("stalled.mp4");
    let started = Instant::now();
    let mut s = FfmpegSink::start(&info, &out, 256, 256, 30, Codec::H264).unwrap();
    s.set_stall_timeout(Duration::from_secs(1));
    let pid = s.pid().unwrap();
    let frame = flat_frame(256, 256, [10, 20, 30, 255]);
    let mut err = None;
    for n in 0..10_000 {
        if let Err(e) = s.push(n, &frame) {
            err = Some(e);
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "still pushing after 10 s");
    }
    let err = err.expect("a stalled ffmpeg must fail a push");
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    let msg = format!("{err:#}");
    assert!(msg.contains("accepted no input"), "{msg}");
    assert_reaped(pid);
    s.abort();
    assert!(dir.path().join("stalled.partial.mp4").exists(), "an ffmpeg failure keeps the partial file: {msg}");
    assert!(!out.exists());
}

#[cfg(unix)]
#[test]
fn an_ffmpeg_that_never_exits_after_its_input_closes_fails_finish() {
    let dir = tempfile::tempdir().unwrap();
    // Reads all of its input, then hangs instead of exiting.
    let fake = fake_ffmpeg_running(dir.path(), "cat >/dev/null\nexec sleep 3600\n");
    let info = sink::detect_ffmpeg_with(&FfmpegChoice::Path(fake)).unwrap();
    let mut s = FfmpegSink::start(&info, &dir.path().join("hung.mp4"), 16, 16, 30, Codec::H264).unwrap();
    s.set_stall_timeout(Duration::from_secs(1));
    let pid = s.pid().unwrap();
    s.push(0, &flat_frame(16, 16, [1, 2, 3, 255])).unwrap();
    let started = Instant::now();
    let msg = format!("{:#}", s.finish().unwrap_err());
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    assert!(msg.contains("did not exit"), "{msg}");
    assert_reaped(pid);
}

#[test]
fn a_frame_of_the_wrong_length_is_rejected() {
    let Some(ff) = ffmpeg() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut s = FfmpegSink::start(&ff, &dir.path().join("short.mp4"), 16, 16, 30, Codec::H264).unwrap();
    let mut f = flat_frame(16, 16, [1, 2, 3, 255]);
    f.rgba.truncate(100);
    assert!(s.push(0, &f).is_err());
    s.abort();
}

#[test]
fn the_command_follows_the_spec_per_codec() {
    let info = |encoders: &[&str]| FfmpegInfo { path: "/x/ffmpeg".into(), version: "v".into(), encoders: encoders.iter().map(|e| (*e).to_string()).collect() };
    let all = info(&["libx264", "h264_videotoolbox", "libsvtav1", "libvpx-vp9"]);
    let line = |i: &FfmpegInfo, out: &str, c: Codec| FfmpegSink::command_line(i, Path::new(out), 1920, 1080, 30, c, sink::Input::Stdin).join(" ");
    assert_eq!(
        line(&all, "o.mp4", Codec::H264),
        "/x/ffmpeg -hide_banner -loglevel error -y -f rawvideo -pix_fmt rgba -s 1920x1080 -r 30 -i - \
         -vf scale=out_color_matrix=bt709:out_range=tv,format=yuv420p,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv -c:v libx264 -preset slow -crf 18 \
         -profile:v high -level:v 4.0 -maxrate 10000k -bufsize 20000k -pix_fmt yuv420p \
         -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv -movflags +faststart -f mp4 -r 30 o.mp4"
    );
    assert!(line(&info(&["h264_videotoolbox"]), "o.mp4", Codec::H264).contains("-c:v h264_videotoolbox -q:v 65 -profile:v high -maxrate 10000k "));
    assert!(line(&all, "o.mp4", Codec::Av1).contains("-c:v libsvtav1 -crf 30 "));
    let vp9 = line(&all, "o.webm", Codec::Vp9);
    assert!(vp9.contains("-c:v libvpx-vp9 -crf 32 -b:v 0 ") && !vp9.contains("faststart"), "{vp9}");
    assert!(FfmpegSink::start(&info(&["libx264"]), Path::new("/nonexistent/o.mp4"), 2, 2, 30, Codec::Av1).is_err());
}

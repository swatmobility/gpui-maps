//! Where a story's frames go: nowhere ([`NullSink`]), one PNG
//! ([`StillSink`]), numbered PNGs ([`PngSequenceSink`]), and the two
//! pictures every run leaves beside its output -- a [`ContactSheet`] of
//! twelve thumbnails and a poster (the middle frame) -- taken by a [`Tee`]
//! in front of the main sink. Video goes through the user's own ffmpeg
//! ([`FfmpegSink`], spawned over a pipe, never linked), or with no ffmpeg
//! to numbered PNGs plus an `encode.sh` ([`EncodeLaterSink`]).
//!
//! No GPU here: thumbnails are scaled with `image`'s `Triangle` filter and
//! the sheet's `t=…s` labels are a 5×7 bitmap font drawn with plain pixel
//! writes, so a sheet is the same bytes on every machine.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use image::{imageops::FilterType, Rgba, RgbaImage};

use crate::capture::Capture;
use crate::story::player::FrameSink;

/// Thumbnails on a contact sheet.
pub const CONTACT_COUNT: usize = 12;
/// A thumbnail's width, px.
pub const THUMB_WIDTH: u32 = 480;
/// Thumbnails per row.
pub const CONTACT_COLUMNS: u32 = 4;
/// The label strip under each thumbnail, px.
pub const LABEL_STRIP: u32 = 16;

/// Takes every frame and keeps none.
pub struct NullSink;

impl FrameSink for NullSink {
    fn push(&mut self, _n: usize, _frame: &Capture) -> anyhow::Result<()> {
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({ "kind": "null" }))
    }
}

/// Frame `n` to `dir/%06d.png`, numbered from the story's frame index (a
/// range re-render overwrites exactly the frames it rendered).
///
/// On [`FrameSink::abort`] the frames already written stay (a later range
/// can fill the rest) and `dir/INCOMPLETE` says so; a finished run removes
/// a stale `INCOMPLETE`.
pub struct PngSequenceSink {
    pub dir: PathBuf,
}

/// The marker an aborted [`PngSequenceSink`] leaves in its directory.
pub const INCOMPLETE_MARKER: &str = "INCOMPLETE";

impl PngSequenceSink {
    /// Where frame `n` goes.
    #[must_use]
    pub fn path_of(&self, n: usize) -> PathBuf {
        self.dir.join(format!("{n:06}.png"))
    }
}

impl FrameSink for PngSequenceSink {
    fn push(&mut self, n: usize, frame: &Capture) -> anyhow::Result<()> {
        save_atomic(frame, &self.path_of(n))
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        let marker = self.dir.join(INCOMPLETE_MARKER);
        if marker.exists() {
            std::fs::remove_file(&marker).with_context(|| format!("removing {}", marker.display()))?;
        }
        Ok(serde_json::json!({ "kind": "png_sequence", "dir": self.dir, "pattern": "%06d.png" }))
    }
    fn abort(&mut self) {
        // Best effort: an abort has no error channel, and the render's own
        // error is the one the caller sees.
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(
            self.dir.join(INCOMPLETE_MARKER),
            "the render failed or was cancelled; the frames here are a partial run\n",
        );
    }
}

/// Exactly one frame, to `path`. A second frame is an error, and so is a
/// run that pushed none. On abort the file this sink wrote is removed.
pub struct StillSink {
    pub path: PathBuf,
    written: Option<usize>,
}

impl StillSink {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path, written: None }
    }
}

impl FrameSink for StillSink {
    fn push(&mut self, n: usize, frame: &Capture) -> anyhow::Result<()> {
        if let Some(first) = self.written {
            anyhow::bail!("a still takes exactly one frame: got {first}, then {n}");
        }
        save_atomic(frame, &self.path)?;
        self.written = Some(n);
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        let n = self.written.ok_or_else(|| anyhow::anyhow!("a still took no frame"))?;
        Ok(serde_json::json!({ "kind": "still", "path": self.path, "frame": n }))
    }
    fn abort(&mut self) {
        if self.written.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `frame` as a PNG at `path`, through a temporary file and a rename, so
/// a crash never leaves a truncated PNG under the final name.
fn save_atomic(frame: &Capture, path: &Path) -> anyhow::Result<()> {
    let image = RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
        .ok_or_else(|| anyhow::anyhow!("frame buffer is not width*height*4 bytes"))?;
    save_image_atomic(&image, path)
}

fn save_image_atomic(image: &RgbaImage, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".partial.png");
    let tmp = PathBuf::from(tmp);
    image.save(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Twelve thumbnails spread evenly over a run's frames, 480 px wide, four
/// to a row, each over a `t=…s` label strip.
pub struct ContactSheet {
    /// The nominal spacing between picked frames (the picks themselves are
    /// rounded so the first and last frames are both on the sheet).
    pub every: usize,
    pub thumb_width: u32,
    pub columns: u32,
    /// `(frame, story seconds, thumbnail)`, in push order.
    tiles: Vec<(usize, f64, RgbaImage)>,
    /// The frames this sheet takes, ascending.
    picks: Vec<usize>,
    fps: u32,
}

impl ContactSheet {
    /// `count` frames evenly spaced over `[0, frames − 1]` (fewer when the
    /// story has fewer frames).
    #[must_use]
    pub fn for_frames(frames: usize, count: usize, fps: u32) -> Self {
        Self::for_range(0..frames, count, fps)
    }

    /// `count` frames evenly spaced over `[range.start, range.end − 1]`: a
    /// sheet of a partial render.
    #[must_use]
    pub fn for_range(range: std::ops::Range<usize>, count: usize, fps: u32) -> Self {
        let len = range.len();
        let count = count.min(len);
        let mut picks: Vec<usize> = match count {
            0 => Vec::new(),
            1 => vec![range.start + (len - 1) / 2],
            _ => (0..count)
                .map(|i| {
                    // Round half up in integers: i*(len-1)/(count-1).
                    let span = (len - 1) * i;
                    range.start + (2 * span + (count - 1)) / (2 * (count - 1))
                })
                .collect(),
        };
        picks.dedup();
        Self {
            every: if count > 1 { (len - 1) / (count - 1) } else { len },
            thumb_width: THUMB_WIDTH,
            columns: CONTACT_COLUMNS,
            tiles: Vec::new(),
            picks,
            fps,
        }
    }

    /// Whether frame `n` goes on the sheet.
    #[must_use]
    pub fn wants(&self, n: usize) -> bool {
        self.picks.binary_search(&n).is_ok()
    }

    /// Frame `n`, `width`×`height` RGBA: kept as a thumbnail when the sheet
    /// wants it (and its buffer is the size it says), else ignored.
    pub fn push_rgba(&mut self, n: usize, width: u32, height: u32, rgba: &[u8]) {
        if !self.wants(n) || self.tiles.iter().any(|(m, ..)| *m == n) || width == 0 || height == 0 {
            return;
        }
        let Some(full) = RgbaImage::from_raw(width, height, rgba.to_vec()) else {
            return;
        };
        // Every thumbnail takes the first one's height, so the grid is
        // regular whatever a later frame's size.
        let thumb_height = self.tiles.first().map_or_else(|| self.thumb_height_for(width, height), |(.., t)| t.height());
        let thumb = image::imageops::resize(&full, self.thumb_width, thumb_height, FilterType::Triangle);
        let t = n as f64 / f64::from(self.fps.max(1));
        self.tiles.push((n, t, thumb));
    }

    fn thumb_height_for(&self, width: u32, height: u32) -> u32 {
        let h = (u64::from(self.thumb_width) * u64::from(height) + u64::from(width) / 2) / u64::from(width);
        u32::try_from(h).unwrap_or(u32::MAX).max(1)
    }

    /// Thumbnails taken so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// The sheet as one image, thumbnails in frame order.
    ///
    /// # Errors
    /// No thumbnail was taken.
    pub fn render(&self) -> anyhow::Result<RgbaImage> {
        let first = self.tiles.first().ok_or_else(|| anyhow::anyhow!("the contact sheet took no frame"))?;
        let th = first.2.height();
        let count = u32::try_from(self.tiles.len())?;
        let columns = self.columns.max(1).min(count);
        let rows = count.div_ceil(columns);
        let cell_h = th + LABEL_STRIP;
        let mut sheet = RgbaImage::from_pixel(columns * self.thumb_width, rows * cell_h, STRIP_BG);
        let mut tiles: Vec<&(usize, f64, RgbaImage)> = self.tiles.iter().collect();
        tiles.sort_by_key(|(n, ..)| *n);
        for (i, (_, t, thumb)) in tiles.into_iter().enumerate() {
            let i = u32::try_from(i)?;
            let (x, y) = ((i % columns) * self.thumb_width, (i / columns) * cell_h);
            image::imageops::replace(&mut sheet, thumb, i64::from(x), i64::from(y));
            draw_text(&mut sheet, x + 6, y + th + 1, &format!("t={t:.2}s"), LABEL_FG);
        }
        Ok(sheet)
    }

    /// The sheet as a PNG at `path`; returns its `(width, height)`.
    ///
    /// # Errors
    /// No thumbnail was taken, or the file could not be written.
    pub fn save(&self, path: &Path) -> anyhow::Result<(u32, u32)> {
        let sheet = self.render()?;
        save_image_atomic(&sheet, path)?;
        Ok(sheet.dimensions())
    }
}

const STRIP_BG: Rgba<u8> = Rgba([24, 24, 24, 255]);
const LABEL_FG: Rgba<u8> = Rgba([235, 235, 235, 255]);
/// The bitmap font's pixel scale: 5×7 cells drawn 10×14, in the 16 px strip.
const GLYPH_SCALE: u32 = 2;

/// A 5×7 glyph, one row per byte, the low five bits left to right.
fn glyph(c: char) -> Option<[u8; 7]> {
    Some(match c {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        '.' => [0, 0, 0, 0, 0, 0b01100, 0b01100],
        's' => [0, 0, 0b01110, 0b10000, 0b01110, 0b00001, 0b11110],
        't' => [0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110],
        '=' => [0, 0, 0b11111, 0, 0b11111, 0, 0],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        _ => return None,
    })
}

/// `text` at `(x, y)` in the bitmap font; characters it has no glyph for
/// are skipped (their advance kept), pixels off the image are dropped.
fn draw_text(image: &mut RgbaImage, x: u32, y: u32, text: &str, color: Rgba<u8>) {
    let advance = 6 * GLYPH_SCALE;
    for (i, c) in text.chars().enumerate() {
        let Some(rows) = glyph(c) else { continue };
        let gx = x + u32::try_from(i).unwrap_or(u32::MAX / advance) * advance;
        for (row, bits) in (0u32..).zip(rows) {
            for col in 0..5u32 {
                if bits & (0b10000 >> col) == 0 {
                    continue;
                }
                for dy in 0..GLYPH_SCALE {
                    for dx in 0..GLYPH_SCALE {
                        let (px, py) = (gx + col * GLYPH_SCALE + dx, y + row * GLYPH_SCALE + dy);
                        if px < image.width() && py < image.height() {
                            image.put_pixel(px, py, color);
                        }
                    }
                }
            }
        }
    }
}

/// Sends every frame to `main`, keeps the contact sheet's thumbnails, and
/// keeps frame `poster_frame` at full size as the poster.
pub struct Tee<'a> {
    pub main: &'a mut dyn FrameSink,
    pub sheet: ContactSheet,
    pub poster_frame: usize,
    pub poster: Option<RgbaImage>,
}

impl<'a> Tee<'a> {
    /// The standard tee over `range` of a story at `fps`: a
    /// [`CONTACT_COUNT`]-thumbnail sheet and the range's middle frame.
    pub fn over(main: &'a mut dyn FrameSink, range: std::ops::Range<usize>, fps: u32) -> Self {
        let poster_frame = range.start + range.len() / 2;
        Self { main, sheet: ContactSheet::for_range(range, CONTACT_COUNT, fps), poster_frame, poster: None }
    }

    /// The poster as a PNG at `path`.
    ///
    /// # Errors
    /// The poster frame never came, or the file could not be written.
    pub fn save_poster(&self, path: &Path) -> anyhow::Result<()> {
        let poster = self.poster.as_ref().ok_or_else(|| anyhow::anyhow!("the poster frame {} never came", self.poster_frame))?;
        save_image_atomic(poster, path)
    }
}

impl FrameSink for Tee<'_> {
    fn push(&mut self, n: usize, frame: &Capture) -> anyhow::Result<()> {
        self.main.push(n, frame)?;
        self.sheet.push_rgba(n, frame.width, frame.height, &frame.rgba);
        if n == self.poster_frame {
            self.poster = Some(
                RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone())
                    .ok_or_else(|| anyhow::anyhow!("frame buffer is not width*height*4 bytes"))?,
            );
        }
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        self.main.finish()
    }
    fn abort(&mut self) {
        self.main.abort();
    }
}

// ---------------------------------------------------------------------
// Video through the user's ffmpeg (spec §5.2). ffmpeg is a separate
// program the user installed, spawned over a pipe: never vendored, linked
// or downloaded, and no codec library enters this crate's dependency tree.
// ---------------------------------------------------------------------

/// The ffmpeg this machine has, as [`detect_ffmpeg`] found it.
///
/// Serialised (a validation report, a manifest) by its file NAME and
/// version only, never its path: a local path is personal data (Task 17
/// ruling), and the version is what identifies the build.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct FfmpegInfo {
    #[serde(rename = "name", serialize_with = "serialize_file_name")]
    pub path: PathBuf,
    /// The first line of `ffmpeg -version`, e.g. `ffmpeg version 8.0.1 …`.
    pub version: String,
    /// The encoder names `-hide_banner -encoders` lists (`libx264`, …).
    pub encoders: Vec<String>,
}

fn serialize_file_name<S: serde::Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&file_name_of(path))
}

/// `path`'s file name (`ffmpeg`), or the whole path when it has none.
fn file_name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl FfmpegInfo {
    /// The binary's file name (`ffmpeg`): what a manifest records.
    #[must_use]
    pub fn name(&self) -> String {
        file_name_of(&self.path)
    }

    /// `{"name", "version"}`: the ffmpeg as a manifest records it.
    #[must_use]
    pub fn manifest_value(&self) -> serde_json::Value {
        serde_json::json!({ "name": self.name(), "version": self.version })
    }

    /// This build lists `encoder`.
    #[must_use]
    pub fn has(&self, encoder: &str) -> bool {
        self.encoders.iter().any(|e| e == encoder)
    }
}

/// Whether an output `format` (the `story` CLI's `--format`) is a video
/// for the publish table's Mapbox video rule (spec §6): `mp4`, `webm`,
/// `png-seq` and `hashes` are (frames of a video), `still`, `png` (a
/// bundle without a video) and `contact-sheet` are not. Any other name is
/// judged a video (the stricter verdict). The one rule a render
/// ([`crate::story::RenderOptions::video`]) and a pre-render judgement
/// (`--validate-only`) share.
#[must_use]
pub fn output_is_video(format: &str) -> bool {
    !matches!(format, "still" | "png" | "contact-sheet")
}

/// Which ffmpeg to use, chosen explicitly so a test never mutates the
/// process environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FfmpegChoice {
    /// `$GPUI_MAP_FFMPEG`, then `ffmpeg` on `PATH` (what `which ffmpeg` finds).
    Detect,
    /// Exactly this binary; a path that does not run is no ffmpeg.
    Path(PathBuf),
}

/// The environment variable naming an ffmpeg binary.
pub const FFMPEG_ENV: &str = "GPUI_MAP_FFMPEG";

/// `$GPUI_MAP_FFMPEG`, then `which ffmpeg`, probed with `-version` and
/// `-hide_banner -encoders`. `None` when neither runs.
#[must_use]
pub fn detect_ffmpeg() -> Option<FfmpegInfo> {
    detect_ffmpeg_with(&FfmpegChoice::Detect)
}

/// [`detect_ffmpeg`] for an explicit [`FfmpegChoice`].
#[must_use]
pub fn detect_ffmpeg_with(choice: &FfmpegChoice) -> Option<FfmpegInfo> {
    match choice {
        FfmpegChoice::Path(p) => probe_ffmpeg(p),
        FfmpegChoice::Detect => {
            if let Some(p) = std::env::var_os(FFMPEG_ENV).filter(|p| !p.is_empty()) {
                // An explicit choice that does not run is no ffmpeg; falling
                // through to PATH would hide the user's mistake.
                return probe_ffmpeg(Path::new(&p));
            }
            which("ffmpeg").and_then(|p| probe_ffmpeg(&p))
        }
    }
}

/// The first executable `name` on `PATH`, as `which` finds it.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn probe_ffmpeg(path: &Path) -> Option<FfmpegInfo> {
    use std::process::{Command, Stdio};
    let version = Command::new(path).arg("-version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !version.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&version.stdout).lines().next().unwrap_or_default().trim().to_string();
    let encoders = Command::new(path)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some(FfmpegInfo { path: path.to_path_buf(), version, encoders: parse_encoders(&String::from_utf8_lossy(&encoders.stdout)) })
}

/// The names in `ffmpeg -encoders` output: after the ` ------` rule, each
/// line is ` FLAGS name  description`.
fn parse_encoders(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !l.trim_start().starts_with("---"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// The video codec (spec §5.2; R8). H.264 carries patent licensing that is
/// the publisher's concern; AV1 and VP9 are the royalty-free options.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    /// HEVC (spec §2.4: optional), tagged `hvc1` in the MP4 so QuickTime
    /// and Apple devices play it (`hev1`, ffmpeg's default tag, they
    /// refuse).
    Hevc,
    Av1,
    Vp9,
}

impl Codec {
    /// The codec a name (`--codec`, `output.codec`) chooses: `h264`,
    /// `hevc`, `av1`, `vp9`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "h264" => Some(Codec::H264),
            "hevc" => Some(Codec::Hevc),
            "av1" => Some(Codec::Av1),
            "vp9" => Some(Codec::Vp9),
            _ => None,
        }
    }

    /// The script's `output.codec` (`h264`, the default, or `hevc`;
    /// parsing refuses any other name).
    #[must_use]
    pub fn of_script(script: &crate::story::Script) -> Self {
        script.output.codec.as_deref().and_then(Self::from_name).unwrap_or(Codec::H264)
    }
}

/// What ffmpeg reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Raw RGBA frames on stdin.
    Stdin,
    /// Numbered PNGs `dir/%06d.png`.
    Frames(PathBuf),
}

/// Spec §5.2's filter plus a `setparams` that tags the frames bt709.
///
/// Without it, ffmpeg 8.0.1 writes `color_primaries=unknown` and
/// `color_trc=unknown` whatever `-color_primaries`/`-color_trc` say: the
/// filtered frames carry the rawvideo input's unspecified primaries and
/// transfer, and those override the output options (measured
/// 2026-09-29 with ffprobe; the matrix and range, which `scale` sets,
/// came out right). The output options stay for ffmpeg builds that honour
/// them.
pub const VIDEO_FILTER: &str = "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p,\
setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709:range=tv";

/// The encoder and its quality arguments for `codec` on this ffmpeg, for
/// a `width`x`height` stream at `fps` (spec §2.4 "Encoding"):
///
/// - H.264: libx264 CRF 18, High profile, the level
///   [`tiers::h264_level`] names for the frame and rate, and a VBV cap at
///   [`tiers::bitrate_kbps`] of the frame's tier (buffer twice that).
///   Without libx264, `h264_videotoolbox -q:v 65` at High profile and
///   the same cap.
/// - HEVC: libx265 CRF 20 under the same cap, tagged `hvc1`. Without
///   libx265, `hevc_videotoolbox -q:v 65`, tagged `hvc1`.
/// - AV1 and VP9 unchanged.
///
/// The tier is keyed off the frame's short edge
/// ([`tiers::tier_for_short_edge`], ruling F15), so a custom size or an
/// old preset gets the rate of the tier it is closest to at or above.
fn encoder_args(info: &FfmpegInfo, codec: Codec, width: u32, height: u32, fps: u32) -> Vec<String> {
    use crate::story::tiers;
    let kbps = tiers::bitrate_kbps(tiers::tier_for_short_edge(width.min(height)), fps);
    let (maxrate, bufsize) = (format!("{kbps}k"), format!("{}k", u64::from(kbps) * 2));
    let v = |xs: &[&str]| xs.iter().map(|s| (*s).to_string()).collect::<Vec<String>>();
    match codec {
        Codec::H264 if !info.has("libx264") && info.has("h264_videotoolbox") => v(&[
            "-c:v",
            "h264_videotoolbox",
            "-q:v",
            "65",
            "-profile:v",
            "high",
            "-maxrate",
            &maxrate,
            "-bufsize",
            &bufsize,
            "-pix_fmt",
            "yuv420p",
        ]),
        Codec::H264 => v(&[
            "-c:v",
            "libx264",
            "-preset",
            "slow",
            "-crf",
            "18",
            "-profile:v",
            "high",
            "-level:v",
            tiers::h264_level(width, height, fps),
            "-maxrate",
            &maxrate,
            "-bufsize",
            &bufsize,
            "-pix_fmt",
            "yuv420p",
        ]),
        Codec::Hevc if !info.has("libx265") && info.has("hevc_videotoolbox") => {
            v(&["-c:v", "hevc_videotoolbox", "-q:v", "65", "-tag:v", "hvc1", "-maxrate", &maxrate, "-bufsize", &bufsize, "-pix_fmt", "yuv420p"])
        }
        Codec::Hevc => v(&[
            "-c:v",
            "libx265",
            "-preset",
            "slow",
            "-crf",
            "20",
            "-tag:v",
            "hvc1",
            "-maxrate",
            &maxrate,
            "-bufsize",
            &bufsize,
            "-pix_fmt",
            "yuv420p",
        ]),
        Codec::Av1 => v(&["-c:v", "libsvtav1", "-crf", "30"]),
        Codec::Vp9 => v(&["-c:v", "libvpx-vp9", "-crf", "32", "-b:v", "0"]),
    }
}

/// The encoder `codec` needs that this ffmpeg lacks, if any, judged from
/// the encoders `ffmpeg -encoders` listed ([`FfmpegInfo::encoders`]).
fn missing_encoder(info: &FfmpegInfo, codec: Codec) -> Option<&'static str> {
    match codec {
        Codec::H264 if !info.has("libx264") && !info.has("h264_videotoolbox") => Some("libx264 or h264_videotoolbox"),
        Codec::Hevc if !info.has("libx265") && !info.has("hevc_videotoolbox") => Some("libx265 or hevc_videotoolbox"),
        Codec::Av1 if !info.has("libsvtav1") => Some("libsvtav1"),
        Codec::Vp9 if !info.has("libvpx-vp9") => Some("libvpx-vp9"),
        _ => None,
    }
}

/// [`FfmpegSink::start`]'s refusal for an ffmpeg without `codec`'s
/// encoder: `None` when it has one.
#[must_use]
pub fn missing_encoder_error(info: &FfmpegInfo, codec: Codec) -> Option<String> {
    missing_encoder(info, codec).map(|needed| {
        format!(
            "{} ({}) has no {needed} encoder for {codec:?} (`ffmpeg -encoders` does not list it); \
             install an ffmpeg built with it, or choose another codec",
            info.name(),
            info.version
        )
    })
}

impl FfmpegSink {
    /// The argv (program first) for spec §5.2's command:
    ///
    /// ```text
    /// ffmpeg -hide_banner -loglevel error -y \
    ///   -f rawvideo -pix_fmt rgba -s {W}x{H} -r {fps} -i - \
    ///   -vf "scale=out_color_matrix=bt709:out_range=tv,format=yuv420p,setparams=…bt709…" \
    ///   -c:v libx264 -preset slow -crf 18 -profile:v high -level:v {level} \
    ///   -maxrate {k}k -bufsize {2k}k -pix_fmt yuv420p \
    ///   -colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv \
    ///   -movflags +faststart -r {fps} {out}
    /// ```
    ///
    /// [`Input::Frames`] reads `-framerate {fps} -i {dir}/%06d.png`
    /// instead. `+faststart` is an MP4 flag and is left off a `.webm`. The
    /// bt709 tags matter: untagged 1080p is guessed as bt601 and the
    /// map's colours shift. See [`VIDEO_FILTER`] for the `setparams` the
    /// spec's command lacks.
    #[must_use]
    pub fn command_line(info: &FfmpegInfo, out: &Path, width: u32, height: u32, fps: u32, codec: Codec, input: Input) -> Vec<String> {
        let mut a: Vec<String> = vec![info.path.display().to_string()];
        let mut push = |xs: &[&str]| a.extend(xs.iter().map(|s| (*s).to_string()));
        push(&["-hide_banner", "-loglevel", "error", "-y"]);
        match &input {
            Input::Stdin => {
                push(&["-f", "rawvideo", "-pix_fmt", "rgba", "-s", &format!("{width}x{height}"), "-r", &fps.to_string(), "-i", "-"]);
            }
            Input::Frames(dir) => {
                push(&["-framerate", &fps.to_string(), "-i", &dir.join("%06d.png").display().to_string()]);
            }
        }
        push(&["-vf", VIDEO_FILTER]);
        a.extend(encoder_args(info, codec, width, height, fps));
        let mut push = |xs: &[&str]| a.extend(xs.iter().map(|s| (*s).to_string()));
        push(&["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]);
        if codec != Codec::Vp9 {
            push(&["-movflags", "+faststart"]);
        }
        // The container comes from the codec, not the file name, so an
        // `out` without an extension (or `x.partial`) still encodes.
        push(&["-f", if codec == Codec::Vp9 { "webm" } else { "mp4" }]);
        push(&["-r", &fps.to_string()]);
        a.push(out.display().to_string());
        a
    }
}

/// Raw RGBA frames into the user's ffmpeg on a dedicated writer thread,
/// through a bounded channel of [`FFMPEG_QUEUE`] frames, so backpressure
/// stalls the player rather than growing memory.
///
/// ffmpeg writes `<stem>.partial.<ext>`; a successful [`FrameSink::finish`]
/// renames it to `out`, so `out` only ever holds a complete video. When
/// ffmpeg fails on its own (a non-zero exit, a closed pipe) the
/// `.partial` file stays for diagnosis and the error carries the last 20
/// lines of ffmpeg's stderr. [`FrameSink::abort`] (cancel, player error)
/// kills ffmpeg and removes the partial file and the stderr log. Dropped
/// without either, the child is killed and the `.partial` file stays.
///
/// An ffmpeg that stops reading without exiting (a stalled hardware
/// encoder, a full disk, a prompt) is killed once it has taken no input
/// for [`FFMPEG_STALL`], and once it has not exited that long after its
/// input closed: `push` and `finish` return an error, never hang, so the
/// player's cancel check and `abort` are reached.
pub struct FfmpegSink {
    child: Option<std::process::Child>,
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    stderr_path: PathBuf,
    partial: PathBuf,
    out: PathBuf,
    width: u32,
    height: u32,
    report: serde_json::Value,
    /// ffmpeg failed on its own: keep the partial file, even through abort.
    failed: bool,
    frames: usize,
    stall_timeout: std::time::Duration,
}

/// Frames buffered between the player and the ffmpeg writer thread.
pub const FFMPEG_QUEUE: usize = 4;
/// Lines of ffmpeg's stderr an error carries.
pub const STDERR_TAIL_LINES: usize = 20;
/// How long ffmpeg may take no input (a full queue), or take to exit after
/// its input closed, before it is killed as stalled.
pub const FFMPEG_STALL: std::time::Duration = std::time::Duration::from_secs(60);
/// How often a blocked `push` or `finish` looks at ffmpeg again.
const STALL_POLL: std::time::Duration = std::time::Duration::from_millis(2);

/// `out` with `.partial` before its extension: `a/e2e.mp4` -> `a/e2e.partial.mp4`.
#[must_use]
pub fn partial_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = match out.extension() {
        Some(ext) => format!("{stem}.partial.{}", ext.to_string_lossy()),
        None => format!("{stem}.partial"),
    };
    out.with_file_name(name)
}

impl FfmpegSink {
    /// Spawn ffmpeg for a `width`×`height` RGBA stream at `fps` into `out`
    /// (written as `<stem>.partial.<ext>` until [`FrameSink::finish`]).
    ///
    /// # Errors
    /// This ffmpeg lacks the codec's encoder; its path or `out` is not
    /// UTF-8 (the argv is built as text); `out`'s directory cannot be
    /// made; ffmpeg or the writer thread does not start.
    pub fn start(info: &FfmpegInfo, out: &Path, width: u32, height: u32, fps: u32, codec: Codec) -> anyhow::Result<Self> {
        use std::process::{Command, Stdio};
        for p in [&info.path, out] {
            if p.to_str().is_none() {
                anyhow::bail!("{} is not UTF-8; the ffmpeg command line is built as text", p.display());
            }
        }
        if let Some(why) = missing_encoder_error(info, codec) {
            anyhow::bail!(why);
        }
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let partial = partial_path(out);
        let mut stderr_path = out.as_os_str().to_owned();
        stderr_path.push(".ffmpeg-stderr.log");
        let stderr_path = PathBuf::from(stderr_path);
        let stderr = std::fs::File::create(&stderr_path).with_context(|| format!("creating {}", stderr_path.display()))?;
        let argv = Self::command_line(info, &partial, width, height, fps, codec, Input::Stdin);
        let (program, args) = argv.split_first().ok_or_else(|| anyhow::anyhow!("empty ffmpeg command"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("starting {}", info.path.display()))?;
        let reap = |mut child: std::process::Child| {
            let _ = child.kill();
            let _ = child.wait();
        };
        let Some(mut stdin) = child.stdin.take() else {
            reap(child);
            anyhow::bail!("ffmpeg has no stdin");
        };
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(FFMPEG_QUEUE);
        let writer = std::thread::Builder::new().name("story-ffmpeg-writer".into()).spawn(move || {
            use std::io::Write as _;
            for frame in rx {
                stdin.write_all(&frame)?;
            }
            stdin.flush()
            // stdin drops here: ffmpeg sees end of input.
        });
        let writer = match writer {
            Ok(w) => w,
            Err(e) => {
                reap(child);
                return Err(anyhow::Error::new(e).context("starting the ffmpeg writer thread"));
            }
        };
        // `command` is the argv that ran, the binary by name (never its
        // path): ffmpeg writes `written_as`, which a successful finish
        // renames to `out`.
        let mut recorded = argv.clone();
        if let Some(program) = recorded.first_mut() {
            *program = info.name();
        }
        let report = serde_json::json!({
            "kind": "ffmpeg",
            "out": out,
            "written_as": partial,
            "codec": codec,
            "ffmpeg": info.manifest_value(),
            "command": recorded,
            "stderr": stderr_path,
        });
        Ok(Self {
            child: Some(child),
            tx: Some(tx),
            writer: Some(writer),
            stderr_path,
            partial,
            out: out.to_path_buf(),
            width,
            height,
            report,
            failed: false,
            frames: 0,
            stall_timeout: FFMPEG_STALL,
        })
    }

    /// ffmpeg's process id while it runs (`None` once reaped).
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(std::process::Child::id)
    }

    /// Replace [`FFMPEG_STALL`], for tests.
    #[doc(hidden)]
    pub fn set_stall_timeout(&mut self, d: std::time::Duration) {
        self.stall_timeout = d;
    }

    /// The last [`STDERR_TAIL_LINES`] lines ffmpeg wrote to stderr.
    fn stderr_tail(&self) -> String {
        let text = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let skip = lines.len().saturating_sub(STDERR_TAIL_LINES);
        lines.iter().skip(skip).copied().collect::<Vec<_>>().join("\n")
    }

    /// Close stdin, wait for ffmpeg to exit (killing it as stalled after
    /// the stall timeout), then join the writer: ffmpeg's exit status, and
    /// the writer's error if the pipe broke.
    ///
    /// ffmpeg exits before the join, so a writer blocked on a pipe that
    /// ffmpeg stopped reading gets `EPIPE` and the join returns.
    fn close_and_wait(&mut self) -> anyhow::Result<(std::process::ExitStatus, Option<std::io::Error>)> {
        drop(self.tx.take());
        let deadline = std::time::Instant::now() + self.stall_timeout;
        let status = loop {
            let child = self.child.as_mut().ok_or_else(|| anyhow::anyhow!("ffmpeg already finished"))?;
            if let Some(status) = child.try_wait().context("waiting for ffmpeg")? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                self.kill();
                return Err(self.failure(format!(
                    "ffmpeg did not exit within {:.1} s of its input closing and was killed",
                    self.stall_timeout.as_secs_f64()
                )));
            }
            std::thread::sleep(STALL_POLL);
        };
        self.child = None;
        let write_err = match self.writer.take().map(std::thread::JoinHandle::join) {
            Some(Ok(Err(e))) => Some(e),
            Some(Err(_)) => Some(std::io::Error::other("the ffmpeg writer thread panicked")),
            _ => None,
        };
        Ok((status, write_err))
    }

    /// ffmpeg failed on its own: keep the partial file and report the
    /// stderr tail.
    fn failure(&mut self, what: String) -> anyhow::Error {
        self.failed = true;
        let tail = self.stderr_tail();
        anyhow::anyhow!(
            "{what}; the partial output is {}; ffmpeg stderr (last {STDERR_TAIL_LINES} lines):\n{tail}",
            self.partial.display()
        )
    }

    fn kill(&mut self) {
        drop(self.tx.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // The writer's pipe is closed now, so the join returns.
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
    }
}

impl FrameSink for FfmpegSink {
    fn push(&mut self, n: usize, frame: &Capture) -> anyhow::Result<()> {
        if (frame.width, frame.height) != (self.width, self.height) {
            anyhow::bail!("frame {n} is {}x{}, the encoder takes {}x{}", frame.width, frame.height, self.width, self.height);
        }
        let expected = self.width as usize * self.height as usize * 4;
        if frame.rgba.len() != expected {
            anyhow::bail!("frame {n} has {} bytes of RGBA, {}x{} needs {expected}", frame.rgba.len(), self.width, self.height);
        }
        use std::sync::mpsc::TrySendError;
        let started = std::time::Instant::now();
        // One copy per frame (8 MB at 1080p): the writer thread needs an
        // owned buffer and `&Capture` is only borrowed.
        let mut buf = frame.rgba.clone();
        loop {
            let tx = self.tx.as_ref().ok_or_else(|| anyhow::anyhow!("the ffmpeg sink is finished"))?;
            match tx.try_send(buf) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => {
                    // The writer stopped: the pipe broke because ffmpeg exited.
                    let (status, write_err) = self.close_and_wait()?;
                    let why = write_err.map(|e| format!(" ({e})")).unwrap_or_default();
                    return Err(self.failure(format!("ffmpeg stopped reading at frame {n} and exited with {status}{why}")));
                }
                Err(TrySendError::Full(back)) => {
                    buf = back;
                    let exited = match self.child.as_mut() {
                        Some(child) => child.try_wait().context("polling ffmpeg")?.is_some(),
                        None => true,
                    };
                    if exited {
                        let (status, write_err) = self.close_and_wait()?;
                        let why = write_err.map(|e| format!(" ({e})")).unwrap_or_default();
                        return Err(self.failure(format!("ffmpeg exited with {status} at frame {n}{why}")));
                    }
                    if started.elapsed() >= self.stall_timeout {
                        self.kill();
                        return Err(self.failure(format!(
                            "ffmpeg accepted no input for {:.1} s at frame {n} and was killed",
                            self.stall_timeout.as_secs_f64()
                        )));
                    }
                    std::thread::sleep(STALL_POLL);
                }
            }
        }
        self.frames += 1;
        Ok(())
    }

    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        let (status, write_err) = self.close_and_wait()?;
        if !status.success() {
            return Err(self.failure(format!("ffmpeg exited with {status} after {} frames", self.frames)));
        }
        if let Some(e) = write_err {
            return Err(self.failure(format!("writing frames to ffmpeg failed: {e}")));
        }
        std::fs::rename(&self.partial, &self.out)
            .with_context(|| format!("renaming {} to {}", self.partial.display(), self.out.display()))?;
        if std::fs::metadata(&self.stderr_path).is_ok_and(|m| m.len() == 0) {
            let _ = std::fs::remove_file(&self.stderr_path);
            if let Some(o) = self.report.as_object_mut() {
                o.remove("stderr");
            }
        }
        let mut report = self.report.clone();
        if let Some(o) = report.as_object_mut() {
            o.insert("frames".into(), self.frames.into());
        }
        Ok(report)
    }

    fn abort(&mut self) {
        self.kill();
        if !self.failed {
            let _ = std::fs::remove_file(&self.partial);
            let _ = std::fs::remove_file(&self.stderr_path);
        }
    }
}

impl Drop for FfmpegSink {
    /// Without `finish` or `abort`: kill ffmpeg; the `.partial` file stays.
    fn drop(&mut self) {
        self.kill();
    }
}

/// Write `dir/<script_name>`: spec §5.2's command over
/// `<frames_dir>/%06d.png` (relative to `dir`, which the script `cd`s
/// into) into `out_name` (a file name in `dir`) with `codec`.
/// `info_or_default` is the ffmpeg to name: a path, or `"ffmpeg"`.
///
/// # Errors
/// The file cannot be written.
#[allow(clippy::too_many_arguments)]
pub fn write_encode_sh(
    dir: &Path,
    script_name: &str,
    frames_dir: &str,
    info_or_default: &str,
    out_name: &str,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
) -> anyhow::Result<PathBuf> {
    let info = FfmpegInfo { path: PathBuf::from(info_or_default), version: String::new(), encoders: vec!["libx264".into(), "libx265".into()] };
    let argv = FfmpegSink::command_line(&info, Path::new(out_name), width, height, fps, codec, Input::Frames(frames_dir.into()));
    let command = argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ");
    let body = format!(
        "#!/bin/sh\n# Encode this story's frames ({width}x{height} at {fps} fps) with your ffmpeg.\nset -e\ncd \"$(dirname \"$0\")\"\n{command}\n"
    );
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(script_name);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(path)
}

/// `s` quoted for `/bin/sh` when it needs it.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=+%,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// With no ffmpeg: numbered PNGs in `dir/<stem>-frames/` and, on finish,
/// `dir/<stem>-encode.sh` with the command that makes `dir/<out_name>`
/// (the script is also printed to stdout). Named after the output, so two
/// renders into one directory never share frames or a script, and a frames
/// directory left by an earlier render of the same output is emptied
/// before the first frame (a shorter render would otherwise leave stale
/// higher-numbered PNGs for `encode.sh` to encode).
pub struct EncodeLaterSink {
    pub frames: PngSequenceSink,
    pub dir: PathBuf,
    /// The video file the script writes, a name in `dir`.
    pub out_name: String,
    /// The script's file name in `dir`.
    pub script_name: String,
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    cleared: bool,
}

impl EncodeLaterSink {
    #[must_use]
    pub fn new(dir: &Path, out_name: &str, codec: Codec, width: u32, height: u32, fps: u32) -> Self {
        let stem = Path::new(out_name).file_stem().map_or_else(|| "story".to_string(), |s| s.to_string_lossy().into_owned());
        Self {
            frames: PngSequenceSink { dir: dir.join(format!("{stem}-frames")) },
            dir: dir.to_path_buf(),
            out_name: out_name.to_string(),
            script_name: format!("{stem}-encode.sh"),
            codec,
            width,
            height,
            fps,
            cleared: false,
        }
    }
}

impl FrameSink for EncodeLaterSink {
    fn push(&mut self, n: usize, frame: &Capture) -> anyhow::Result<()> {
        if !self.cleared {
            // Only the directory this sink names for itself (`<stem>-frames`).
            if self.frames.dir.is_dir() {
                std::fs::remove_dir_all(&self.frames.dir).with_context(|| format!("clearing {}", self.frames.dir.display()))?;
            }
            self.cleared = true;
        }
        self.frames.push(n, frame)
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        let seq = self.frames.finish()?;
        let frames_rel = self.frames.dir.file_name().map_or_else(|| "frames".into(), |n| n.to_string_lossy().into_owned());
        let script = write_encode_sh(
            &self.dir,
            &self.script_name,
            &frames_rel,
            "ffmpeg",
            &self.out_name,
            self.codec,
            self.width,
            self.height,
            self.fps,
        )?;
        let body = std::fs::read_to_string(&script)?;
        let command = body.lines().last().unwrap_or_default().to_string();
        #[allow(clippy::print_stdout)]
        {
            println!("no ffmpeg found (set {FFMPEG_ENV}); frames are in {}; encode them with:\n  {}", self.frames.dir.display(), script.display());
            println!("{command}");
        }
        Ok(serde_json::json!({
            "kind": "png_sequence_encode_later",
            "frames": seq,
            "encode_sh": script,
            "command": command,
            "video": self.dir.join(&self.out_name),
        }))
    }
    fn abort(&mut self) {
        // A render cancelled before its first frame wrote nothing: leave no
        // frames directory behind, not even the partial-run marker.
        if self.cleared {
            self.frames.abort();
        }
    }
}

/// The video sink for `choice`: an [`FfmpegSink`] into `out` when an
/// ffmpeg is found, otherwise an [`EncodeLaterSink`] in `out`'s directory,
/// named after `out`.
///
/// # Errors
/// See [`FfmpegSink::start`].
pub fn video_sink(choice: &FfmpegChoice, out: &Path, width: u32, height: u32, fps: u32, codec: Codec) -> anyhow::Result<Box<dyn FrameSink>> {
    match detect_ffmpeg_with(choice) {
        Some(info) => Ok(Box::new(FfmpegSink::start(&info, out, width, height, fps, codec)?)),
        None => {
            let dir = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let name = out.file_name().and_then(|n| n.to_str()).unwrap_or("story.mp4");
            Ok(Box::new(EncodeLaterSink::new(dir, name, codec, width, height, fps)))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn tiny_capture() -> Capture {
        Capture {
            width: 2,
            height: 2,
            rgba: vec![255; 16],
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
    fn twelve_evenly_spaced_frames_in_a_four_by_three_grid() {
        let s = ContactSheet::for_frames(90, 12, 30);
        let picked: Vec<usize> = (0..90).filter(|&n| s.wants(n)).collect();
        assert_eq!(picked.len(), 12);
        assert_eq!((picked[0], picked[11]), (0, 89));
        // Even: every gap is 8 or 9 frames (89/11 = 8.09).
        assert!(picked.windows(2).all(|w| (8..=9).contains(&(w[1] - w[0]))), "{picked:?}");
    }

    #[test]
    fn the_sheet_is_the_grid_size_and_has_every_thumbnail() {
        let mut s = ContactSheet::for_frames(12, 12, 10);
        for n in 0..12 {
            s.push_rgba(n, 960, 540, &vec![(n * 20) as u8; 960 * 540 * 4]);
        }
        let dir = tempfile::tempdir().unwrap();
        let (w, h) = s.save(&dir.path().join("c.png")).unwrap();
        assert_eq!(w, 4 * 480);
        assert_eq!(h, 3 * (270 + 16));
        // Every thumbnail is on it: cell i's centre is frame i's grey.
        let sheet = image::open(dir.path().join("c.png")).unwrap().to_rgba8();
        for i in 0..12u32 {
            let px = sheet.get_pixel((i % 4) * 480 + 240, (i / 4) * 286 + 135);
            assert_eq!(px.0[0], (i * 20) as u8, "thumbnail {i}");
        }
        // And a label: some label-coloured pixel in cell 0's strip.
        let lit = (0..480).flat_map(|x| (270..286).map(move |y| (x, y))).any(|(x, y)| *sheet.get_pixel(x, y) == LABEL_FG);
        assert!(lit, "no t=…s label drawn");
    }

    #[test]
    fn the_png_sequence_numbers_from_the_frame_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = PngSequenceSink { dir: dir.path().into() };
        s.push(7, &tiny_capture()).unwrap();
        assert!(dir.path().join("000007.png").exists());
        assert!(!dir.path().join("000007.png.partial.png").exists());
    }

    #[test]
    fn an_aborted_sequence_is_marked_and_a_finished_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = PngSequenceSink { dir: dir.path().into() };
        s.push(0, &tiny_capture()).unwrap();
        s.abort();
        assert!(dir.path().join(INCOMPLETE_MARKER).exists());
        assert!(dir.path().join("000000.png").exists(), "an abort keeps the frames");
        s.push(1, &tiny_capture()).unwrap();
        s.finish().unwrap();
        assert!(!dir.path().join(INCOMPLETE_MARKER).exists());
    }

    #[test]
    fn a_still_takes_exactly_one_frame_and_an_abort_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("still.png");
        let mut s = StillSink::new(path.clone());
        assert!(s.finish().is_err(), "no frame is an error");
        s.push(3, &tiny_capture()).unwrap();
        assert!(s.push(4, &tiny_capture()).is_err());
        assert_eq!(s.finish().unwrap()["frame"], 3);
        s.abort();
        assert!(!path.exists());
    }

    #[test]
    fn short_stories_pick_each_frame_once_and_a_range_is_offset() {
        let s = ContactSheet::for_frames(5, 12, 10);
        assert_eq!((0..5).filter(|&n| s.wants(n)).count(), 5);
        let r = ContactSheet::for_range(100..190, 12, 30);
        let picked: Vec<usize> = (0..300).filter(|&n| r.wants(n)).collect();
        assert_eq!((picked.len(), picked[0], picked[11]), (12, 100, 189));
        assert!(ContactSheet::for_frames(0, 12, 30).render().is_err());
    }

    #[test]
    fn the_tee_feeds_main_sheet_and_poster_and_passes_abort_on() {
        struct Count(usize, bool);
        impl FrameSink for Count {
            fn push(&mut self, _: usize, _: &Capture) -> anyhow::Result<()> {
                self.0 += 1;
                Ok(())
            }
            fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!(self.0))
            }
            fn abort(&mut self) {
                self.1 = true;
            }
        }
        let mut main = Count(0, false);
        {
            let mut tee = Tee::over(&mut main, 0..21, 10);
            assert_eq!(tee.poster_frame, 10);
            for n in 0..21 {
                tee.push(n, &tiny_capture()).unwrap();
            }
            assert_eq!(tee.sheet.len(), 12);
            assert!(tee.poster.is_some());
            assert_eq!(tee.finish().unwrap(), serde_json::json!(21));
            tee.abort();
        }
        assert!(main.1);
    }

    #[test]
    fn the_bitmap_font_has_every_label_character() {
        for c in "t=0123456789.s".chars() {
            assert!(glyph(c).is_some(), "{c}");
        }
    }

    fn fake_info(encoders: &[&str]) -> FfmpegInfo {
        FfmpegInfo { path: "/x/ffmpeg".into(), version: "v".into(), encoders: encoders.iter().map(|e| (*e).to_string()).collect() }
    }

    #[test]
    fn a_4k_h264_command_line_has_high_profile_and_level_5_1() {
        let args = FfmpegSink::command_line(&fake_info(&["libx264"]), Path::new("/tmp/x.mp4"), 3840, 2160, 30, Codec::H264, Input::Stdin).join(" ");
        assert!(args.contains("-profile:v high") && args.contains("-level:v 5.1") && args.contains("-maxrate 35000k"), "{args}");
        assert!(args.contains("-bufsize 70000k -pix_fmt yuv420p"), "{args}");
        let portrait = FfmpegSink::command_line(&fake_info(&["libx264"]), Path::new("/tmp/x.mp4"), 1080, 1920, 30, Codec::H264, Input::Stdin).join(" ");
        assert!(portrait.contains("-level:v 4.0 -maxrate 10000k"), "{portrait}");
        // The bt709 tags stay (Task 1, F15).
        assert!(args.contains("-colorspace bt709 -color_primaries bt709 -color_trc bt709 -color_range tv"), "{args}");
    }

    #[test]
    fn hevc_without_libx265_is_the_missing_encoder_error() {
        let info = fake_info(&["libx264", "h264_videotoolbox"]);
        let err = FfmpegSink::start(&info, Path::new("/nonexistent/o.mp4"), 2, 2, 30, Codec::Hevc).err().expect("refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("libx265 or hevc_videotoolbox") && msg.contains("Hevc") && msg.contains("ffmpeg -encoders"), "{msg}");
        assert!(!msg.contains("/x/"), "the error names the binary, not its path: {msg}");
        assert!(missing_encoder_error(&fake_info(&["hevc_videotoolbox"]), Codec::Hevc).is_none());
        assert!(missing_encoder_error(&fake_info(&["libx265"]), Codec::Hevc).is_none());
        // No `.partial` or stderr log was made for a refused codec.
        assert!(!Path::new("/nonexistent").exists());
    }

    #[test]
    fn hevc_is_libx265_tagged_hvc1_under_the_tier_cap() {
        let line = |enc: &[&str]| FfmpegSink::command_line(&fake_info(enc), Path::new("o.mp4"), 3840, 2160, 60, Codec::Hevc, Input::Stdin).join(" ");
        let x265 = line(&["libx265", "hevc_videotoolbox"]);
        assert!(x265.contains("-c:v libx265 -preset slow -crf 20 -tag:v hvc1 -maxrate 52500k -bufsize 105000k"), "{x265}");
        assert!(x265.contains("-movflags +faststart -f mp4"), "{x265}");
        let vt = line(&["hevc_videotoolbox"]);
        assert!(vt.contains("-c:v hevc_videotoolbox -q:v 65 -tag:v hvc1"), "{vt}");
        assert_eq!(Codec::from_name("hevc"), Some(Codec::Hevc));
        assert_eq!(serde_json::to_value(Codec::Hevc).unwrap(), "hevc");
    }
}

//! **Renders a map story (a JSON script) headlessly: an MP4, PNGs, a
//! contact sheet, a still, or frame hashes.** No window, no `gpui`.
//!
//! ```text
//! cargo run --release --example story -- <script.json> [--out PATH] [--preset P]
//!     [--aspect 16:9|9:16|4:5|1:1] [--tier hd|fullhd|qhd|4k] [--safe-area tiktok|reels|shorts|all|youtube|none]
//!     [--format mp4|png-seq|contact-sheet|still|hashes] [--at SECONDS] [--frames A..B]
//!     [--strict] [--offline] [--fixture] [--keyless] [--cache DIR] [--codec h264|hevc|av1|vp9]
//!     [--validate-only] [--bench] [--gate] [--allow-repo-output] [--system-fonts] [--allow-missing] [--bundle DIR]
//! ```
//!
//! | Flag | Meaning |
//! |---|---|
//! | `--out PATH` | Where the output goes. Default `~/Movies/gpui-map/<title>-<utc>.<ext>`. Refused inside a git work tree unless `--allow-repo-output` (rendered frames of licensed tiles are licensed output). |
//! | `--preset P` | `1080p30`, `1080p60`, `4k30`, `square1080`, `vertical1080`, `still4k`: replaces the script's `output` size and rate. |
//! | `--aspect A`, `--tier T` | The frame's shape and resolution tier (spec §2.4): replace the script's `output` size (its `preset`, `width`, `height`, `pixel_ratio`), keeping its rate, so one story renders at 9:16 1080p and at 16:9 4K. The one not given keeps the script's own. Not with `--preset`. |
//! | `--safe-area S` | The short-form app whose UI layout avoids: replaces `output.safe_area`. |
//! | `--format` | `mp4` (default; `png-seq` and an `encode.sh` when no ffmpeg), `png-seq` (a directory of numbered PNGs), `contact-sheet` (twelve frames, rendered alone, on one PNG), `still` (one frame, `--at`), `hashes` (`n sha256` per frame on stdout, nothing written), `png` (with `--bundle` only: the bundle without a video). |
//! | `--bundle DIR` | Write the render bundle (spec §6) into `DIR`: `<stem>.mp4` and `<stem>.story.json` (`--format mp4`, the default; none with `--format png`), then `media/`, `stills/<chapter>.png`, `poster.png`, `<stem>.srt`, `<stem>.vtt` and `transcript.html` (`story::write_bundle`). `<stem>` is the title's slug. `--format mp4 --out DIR/` (a trailing slash, or an existing folder) is the same. Not with `--frames` or `--at`. |
//! | `--at S` | The still's story second (default: the middle). With another format, render only that frame. Not with `--frames`. |
//! | `--frames A..B` | Render only frames `A..B`. When a manifest from an earlier render already sits at this output's manifest path, the range draws the sprite sheet it recorded (still cached), so spliced frames' icons match; a sheet no longer cached is named in the gaps. |
//! | `--strict` | A frame with a missing tile is an error (also on when the script's `output.strict` is `true`). |
//! | `--offline` | Skip the source's reachability check, and take the style's sprite sheet (the icons) from its cache (`~/.cache/gpui-map/sprites/`) only, never the network, whatever its age. Online, a cached sheet older than 7 days is fetched again; delete its folder to refresh it sooner. |
//! | `--fixture` | Swap the basemap for the vendored Singapore fixture tile (tests only; the manifest's source is `fixture`). |
//! | basemaps | `scene.basemap`, first choices first: `{"source": "openfreemap"}`; Protomaps (`{"source": "pmtiles:<url or path>"}` on protomaps.com, or an archive crediting Protomaps/OpenStreetMap); `{"source": "mapbox"}` (a token and a wordmark you supply; a video also needs `output.acknowledge: ["mapbox-video"]`). `--help` lists them in this order. |
//! | `--keyless` | The `-ofm` twin: Liberty on OpenFreeMap instead of Mapbox Streets, with the events' Mapbox layer ids mapped to Liberty's. No token needed. A Mapbox script turns keyless by itself when there is no token or its Mapbox style is not in the checkout (the open library tree), saying so on stderr. |
//! | `--cache DIR` | The raw-tile cache (default `~/.cache/gpui-map/tiles/<source>`), and `DIR/story-images` the image cache validation and the render share (default `~/.cache/gpui-map/story-images`). |
//! | `--codec` | `h264` (High profile, the level by resolution and rate), `hevc` (tagged `hvc1`; needs libx265 or VideoToolbox in your ffmpeg), `av1`, `vp9` (a `.webm`). Default: the script's `output.codec`, else `h264`. |
//! | `--validate-only` | Print the validation report as JSON; exit 2 on any error. |
//! | `--gate` | The perf gate's `story` section (`examples/perf_gate.rs`, `docs/perf-gate.md`): the script (default `tests/fixtures/stories/det.json`) on the fixture tile, rendered to a null sink at 16:9 4K and, with a drawn route added, at 16:9 1080p; prints `gate: story.<name> <value> <unit>` lines. Writes nothing. |
//! | `--bench` | Render the script (default `examples/stories/orbit-marina.json`) at 1080p30 to a null sink, then to ffmpeg, and print fps and each stage's p50/p95. |
//! | `--system-fonts` | Shape labels with the machine's fonts instead of the shipped Noto faces (not reproducible across machines). |
//! | `--allow-missing` | Exit 0 even when a frame is missing tiles (default: exit 3 after writing everything). |
//!
//! Exit codes:
//!
//! | Code | Meaning |
//! |---|---|
//! | 0 | Rendered (or `--validate-only` found no error) |
//! | 1 | The render or its I/O failed (including an output refused inside a git work tree) |
//! | 2 | A usage mistake (unknown flag, bad value), or the script does not parse or validate |
//! | 3 | Rendered, but some frame is missing tiles (0 with `--allow-missing`) |
//! | 130 | Cancelled with Ctrl-C (the partial video is removed) |
//!
//! Relative paths in the script (its style, images, overlay data, tracks,
//! wordmark) resolve against the script file's own folder, so a story
//! renders the same from any current directory. Progress goes to stderr once a second;
//! the manifest's path is the last line on stdout. Every render except
//! `hashes` writes `<out stem>.story.json` (the manifest) next to the
//! output, plus a poster (the middle frame) and a twelve-frame contact
//! sheet in `<out stem>-extras/`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use gpui_maps::story::{self, sink, FrameSink, Player, Progress, RenderManifest, RenderOptions};

const USAGE: &str = "usage: story <script.json> [--out PATH] [--preset P] [--aspect A] [--tier T] [--safe-area S] [--format mp4|png-seq|contact-sheet|still|hashes|png] \
[--at SECONDS] [--frames A..B] [--strict] [--offline] [--fixture] [--keyless] [--cache DIR] [--codec h264|hevc|av1|vp9] \
[--validate-only] [--bench] [--gate] [--allow-repo-output] [--system-fonts] [--allow-missing] [--bundle DIR]

basemaps (scene.basemap), first choices first:
  {\"source\": \"openfreemap\"}                     OpenStreetMap data, no key, publishable in video
  {\"source\": \"pmtiles:<url or path>\"}            Protomaps (a protomaps.com URL, or an archive crediting Protomaps/OSM)
  {\"source\": \"mapbox\"}                          needs MAPBOX_ACCESS_TOKEN and a wordmark you supply; in a video,
                                                  also output.acknowledge [\"mapbox-video\"] (your agreement permits it)";

/// The story `--gate` measures when no script is named: the hermetic
/// determinism fixture (fixture tiles, no network).
const GATE_STORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stories/det.json");

/// The story `--bench` renders when no script is named.
const BENCH_STORY: &str = "examples/stories/orbit-marina.json";

/// The Mapbox Streets layer ids the example stories edit, and the Liberty
/// layers that draw the same features (`--keyless`). An id not listed is
/// kept as it is.
const KEYLESS_LAYERS: &[(&str, &[&str])] = &[
    ("poi-label", &["poi_r20", "poi_r7", "poi_r1"]),
    ("road-primary", &["road_trunk_primary"]),
    ("road-label", &["highway-name-major", "highway-name-minor"]),
    ("water", &["water"]),
    ("building", &["building"]),
];

#[derive(Default, Clone)]
struct Args {
    script: Option<PathBuf>,
    out: Option<PathBuf>,
    preset: Option<String>,
    aspect: Option<story::tiers::Aspect>,
    tier: Option<story::tiers::Tier>,
    safe_area: Option<story::tiers::SafeArea>,
    format: Option<String>,
    at: Option<f64>,
    frames: Option<std::ops::Range<usize>>,
    strict: bool,
    offline: bool,
    fixture: bool,
    keyless: bool,
    cache: Option<PathBuf>,
    codec: Option<String>,
    validate_only: bool,
    bench: bool,
    gate: bool,
    allow_repo_output: bool,
    system_fonts: bool,
    allow_missing: bool,
    bundle: Option<PathBuf>,
    help: bool,
}

/// A command-line mistake: exit 2 with the usage line.
#[derive(Debug)]
struct Usage(String);
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n{USAGE}", self.0)
    }
}
impl std::error::Error for Usage {}

fn usage(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Usage(msg.into()))
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Args> {
    let mut a = Args::default();
    let mut it = argv.into_iter();
    let value = |it: &mut dyn Iterator<Item = String>, name: &str| it.next().ok_or_else(|| usage(format!("{name} needs a value")));
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => a.out = Some(value(&mut it, "--out")?.into()),
            "--preset" => a.preset = Some(value(&mut it, "--preset")?),
            "--aspect" => a.aspect = Some(named(&value(&mut it, "--aspect")?, "--aspect", "16:9, 9:16, 4:5 or 1:1")?),
            "--tier" => a.tier = Some(named(&value(&mut it, "--tier")?, "--tier", "hd, fullhd, qhd or 4k")?),
            "--safe-area" => a.safe_area = Some(named(&value(&mut it, "--safe-area")?, "--safe-area", "tiktok, reels, shorts, all, youtube or none")?),
            "--format" => a.format = Some(value(&mut it, "--format")?),
            "--at" => {
                let v = value(&mut it, "--at")?;
                a.at = Some(v.parse().map_err(|e| usage(format!("--at takes seconds, got `{v}`: {e}")))?);
            }
            "--frames" => {
                let v = value(&mut it, "--frames")?;
                let bad = |e: String| usage(format!("--frames takes A..B (two frame numbers), got `{v}`: {e}"));
                let (s, e) = v.split_once("..").ok_or_else(|| bad("no `..`".into()))?;
                a.frames = Some(s.parse().map_err(|x: std::num::ParseIntError| bad(x.to_string()))?..e.parse().map_err(|x: std::num::ParseIntError| bad(x.to_string()))?);
            }
            "--cache" => a.cache = Some(value(&mut it, "--cache")?.into()),
            "--bundle" => a.bundle = Some(value(&mut it, "--bundle")?.into()),
            "--codec" => a.codec = Some(value(&mut it, "--codec")?),
            "--strict" => a.strict = true,
            "--offline" => a.offline = true,
            "--fixture" => a.fixture = true,
            "--keyless" => a.keyless = true,
            "--validate-only" => a.validate_only = true,
            "--bench" => a.bench = true,
            "--gate" => a.gate = true,
            "--allow-repo-output" => a.allow_repo_output = true,
            "--system-fonts" => a.system_fonts = true,
            "--allow-missing" => a.allow_missing = true,
            "-h" | "--help" => a.help = true,
            s if s.starts_with("--") => return Err(usage(format!("unknown flag `{s}`"))),
            s => {
                if a.script.is_some() {
                    return Err(usage(format!("one script only (got a second: `{s}`)")));
                }
                a.script = Some(s.into());
            }
        }
    }
    if a.preset.is_some() && (a.aspect.is_some() || a.tier.is_some()) {
        return Err(usage("--preset and --aspect/--tier both choose the frame size; give one"));
    }
    if a.frames.is_some() && a.at.is_some() {
        return Err(usage("--frames and --at both choose the frames; give one"));
    }
    if a.bundle.is_some() && (a.frames.is_some() || a.at.is_some()) {
        return Err(usage("--bundle renders the whole story; drop --frames/--at"));
    }
    Ok(a)
}

/// A `--aspect`/`--tier`/`--safe-area` value by its script spelling
/// (`"9:16"`, `"4k"`, `"reels"`); anything else is a usage mistake.
fn named<T: serde::de::DeserializeOwned>(v: &str, flag: &str, takes: &str) -> anyhow::Result<T> {
    serde_json::from_value(serde_json::Value::String(v.to_string())).map_err(|_| usage(format!("{flag} takes {takes}, got `{v}`")))
}

/// The Liberty style the keyless twin draws with (this repository's copy).
const LIBERTY_STYLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/liberty-style.json");

/// Why a script written for Mapbox must render as its keyless twin even
/// without `--keyless`, or `None` when it can render as written. Like
/// `kit::mapbox_or_keyless` for pages: a `"source": "mapbox"` script falls
/// back when there is no token (Mapbox is off unless
/// `GPUI_MAP_ALLOW_MAPBOX=1`, see [`main`]), and any script falls back when
/// its Mapbox-authored style is not in this checkout (the open library tree
/// has none; `scripts/fetch-mapbox-styles.sh` fetches them). `fixture`
/// replaces the basemap, so only a missing style matters then.
fn keyless_fallback(raw: &[u8], story_dir: &Path, token: Option<&str>, fixture: bool) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let mapbox = v.pointer("/scene/basemap/source").and_then(|s| s.as_str()) == Some("mapbox");
    let style = v.pointer("/scene/style").and_then(|s| s.as_str()).unwrap_or_default();
    let mapbox_style = Path::new(style).file_name().is_some_and(|f| f.to_string_lossy().starts_with("mapbox-"));
    if !mapbox && !mapbox_style {
        return None;
    }
    if mapbox_style && !story_dir.join(style).is_file() {
        return Some(format!("{style} is not in this checkout (scripts/fetch-mapbox-styles.sh fetches it)"));
    }
    if mapbox && !fixture && token.is_none_or(|t| t.trim().is_empty()) {
        return Some("no Mapbox token (MAPBOX_ACCESS_TOKEN with GPUI_MAP_ALLOW_MAPBOX=1)".to_string());
    }
    None
}

/// `args` with `keyless` set when [`keyless_fallback`] says the script
/// needs it, saying why on stderr.
fn with_keyless_fallback(args: &Args, raw: &[u8], story_dir: &Path) -> Args {
    let mut out = args.clone();
    if !args.keyless {
        let token = std::env::var("MAPBOX_ACCESS_TOKEN").ok();
        if let Some(why) = keyless_fallback(raw, story_dir, token.as_deref(), args.fixture) {
            eprintln!("story: {why}; drawing the keyless twin (Liberty on OpenFreeMap), as --keyless would");
            out.keyless = true;
        }
    }
    out
}

/// The folder a script's relative paths (style, images, overlay data,
/// tracks, wordmark) resolve against: the script FILE's own folder,
/// canonicalised -- never the current directory, so a story renders the
/// same from wherever it is run (spec §2.2, §4.3).
///
/// # Errors
/// The folder does not exist or cannot be read.
fn story_dir(script_path: &Path) -> anyhow::Result<PathBuf> {
    let parent = script_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    parent.canonicalize().map_err(|e| anyhow::anyhow!("reading the story's folder {}: {e}", parent.display()))
}

/// `to` as a path relative to the folder `from` (both absolute and
/// canonical): `..` up to their common ancestor, then down to `to`.
fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let (a, b): (Vec<_>, Vec<_>) = (from.components().collect(), to.components().collect());
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..a.len() {
        out.push("..");
    }
    for c in b.iter().skip(common) {
        out.push(c.as_os_str());
    }
    out
}

/// The script's JSON with the command line's substitutions made: the
/// preset, the keyless twin, the fixture source. `story_dir` is where the
/// script's relative paths resolve ([`story_dir`]): the keyless twin's
/// style is written relative to it.
fn transform(raw: &[u8], args: &Args, story_dir: &Path) -> anyhow::Result<Vec<u8>> {
    if args.preset.is_none() && !args.keyless && !args.fixture && args.aspect.is_none() && args.tier.is_none() && args.safe_area.is_none() {
        return Ok(raw.to_vec());
    }
    let mut v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| anyhow::anyhow!("the script is not JSON: {e}"))?;
    if args.aspect.is_some() || args.tier.is_some() {
        // The one not given keeps the script's own (an old `vertical1080`
        // script given `--tier 4k` stays 9:16); a script that does not
        // parse keeps the defaults, and fails below as it would anyway.
        let own = story::parse(raw).ok().map(|s| (s.format(), s.output().fps));
        let aspect = args.aspect.or(own.map(|(f, _)| f.aspect)).unwrap_or(story::tiers::Aspect::Landscape);
        let tier = args.tier.or(own.map(|(f, _)| f.tier)).unwrap_or(story::tiers::Tier::Fullhd);
        if !v["output"].is_object() {
            v["output"] = serde_json::json!({});
        }
        if let Some(o) = v["output"].as_object_mut() {
            for k in ["preset", "width", "height", "pixel_ratio"] {
                o.remove(k);
            }
            if let Some((_, fps)) = own {
                o.insert("fps".into(), fps.into());
            }
            o.insert("aspect".into(), serde_json::to_value(aspect)?);
            o.insert("tier".into(), serde_json::to_value(tier)?);
        }
    }
    if let Some(area) = args.safe_area {
        if !v["output"].is_object() {
            v["output"] = serde_json::json!({});
        }
        v["output"]["safe_area"] = serde_json::to_value(area)?;
    }
    if let Some(preset) = &args.preset {
        let strict = v.pointer("/output/strict").cloned();
        let mut output = serde_json::json!({ "preset": preset });
        if let Some(s) = strict {
            output["strict"] = s;
        }
        v["output"] = output;
    }
    if args.keyless {
        let liberty_path = Path::new(LIBERTY_STYLE).canonicalize().map_err(|e| anyhow::anyhow!("reading {LIBERTY_STYLE}: {e}"))?;
        let liberty: Option<serde_json::Value> = std::fs::read(&liberty_path).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let style_ref = relative_path(story_dir, &liberty_path).to_string_lossy().into_owned();
        let ids: Vec<String> = liberty
            .as_ref()
            .and_then(|l| l.get("layers"))
            .and_then(|l| l.as_array())
            .map(|ls| ls.iter().filter_map(|l| l.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
            .unwrap_or_default();
        for layer in keyless(&mut v, &ids, &style_ref) {
            eprintln!(
                "story: --keyless: the Mapbox Streets layer `{layer}` has no Liberty counterpart in the keyless map \
                 (KEYLESS_LAYERS in examples/story.rs); its event will fail validation -- add a mapping or drop the event"
            );
        }
    }
    if args.fixture {
        v["scene"]["basemap"] = serde_json::json!({ "source": "fixture" });
    }
    Ok(serde_json::to_vec(&v)?)
}

/// The `-ofm` twin: Liberty on OpenFreeMap, each event naming a Mapbox
/// layer repeated for every Liberty layer that draws the same features.
/// Returns the layers events name that are neither mapped nor among
/// `liberty_ids` (they will fail validation; the caller says why).
/// `style` is the Liberty style's path as the script names it (relative
/// to the story's folder).
fn keyless(v: &mut serde_json::Value, liberty_ids: &[String], style: &str) -> Vec<String> {
    let mut unmapped = Vec::new();
    v["scene"]["style"] = style.into();
    v["scene"]["basemap"] = serde_json::json!({ "source": "openfreemap" });
    // Review fix round 1, Finding 1 (out-of-diff root cause for Task 20's
    // controller-observed poster.png): a script's static `scene.credits`
    // survives `Player::verdict` (`src/story/player.rs`) chained onto
    // whatever the LIVE source's own stated credit is, with no
    // basemap-consistency check -- so a script written against Mapbox and
    // then rendered `--keyless` used to burn in BOTH the correct live
    // OpenFreeMap credit and the now-stale Mapbox one. `keyless` already
    // rewrites `scene.style`/`scene.basemap`; it must rewrite `scene.credits`
    // the same way, or a keyless render keeps crediting a basemap it never
    // drew a single tile of.
    if let Some(credits) = v.get_mut("scene").and_then(|s| s.get_mut("credits")).and_then(|c| c.as_array_mut()) {
        credits.retain(|c| !c.as_str().is_some_and(|s| s.contains("Mapbox")));
    }
    if let Some(title) = v.get("title").and_then(|t| t.as_str()).map(str::to_string) {
        v["title"] = format!("{title} (OpenFreeMap)").into();
    }
    let Some(events) = v.get_mut("events").and_then(|e| e.as_array_mut()) else { return unmapped };
    let mut out = Vec::with_capacity(events.len());
    for event in events.drain(..) {
        let layer_at = event.as_object().and_then(|o| {
            ["set_layer_visibility", "set_filter", "set_paint_property", "set_layout_property", "tween_paint_property"]
                .iter()
                .find_map(|k| o.get(*k).and_then(|a| a.get("layer")).and_then(|l| l.as_str()).map(|l| (*k, l.to_string())))
        });
        match layer_at.as_ref().and_then(|(k, layer)| KEYLESS_LAYERS.iter().find(|(m, _)| *m == layer.as_str()).map(|(_, to)| (*k, *to))) {
            Some((key, targets)) => {
                for target in targets {
                    let mut e = event.clone();
                    e[key]["layer"] = (*target).into();
                    out.push(e);
                }
            }
            None => {
                if let Some((_, layer)) = layer_at {
                    if !liberty_ids.contains(&layer) && !unmapped.contains(&layer) {
                        unmapped.push(layer);
                    }
                }
                out.push(event);
            }
        }
    }
    *events = out;
    unmapped
}

/// `s` as a file-name slug: lowercase ASCII letters, digits and dashes.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Now as `YYYYMMDDTHHMMSSZ`.
fn utc_stamp() -> String {
    stamp_of(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64)
}

/// Unix seconds `secs` as `YYYYMMDDTHHMMSSZ`.
fn stamp_of(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// `--codec`, else the script's `output.codec`, else H.264.
fn codec_of(name: Option<&str>, script: &story::Script) -> anyhow::Result<sink::Codec> {
    match name {
        None => Ok(sink::Codec::of_script(script)),
        Some(n) => sink::Codec::from_name(n).ok_or_else(|| usage(format!("--codec takes h264, hevc, av1 or vp9, got `{n}`"))),
    }
}

/// `<out stem>.story.json` next to `out`.
fn manifest_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().map_or_else(|| "story".into(), |s| s.to_string_lossy().into_owned());
    out.with_file_name(format!("{stem}.story.json"))
}

/// `<out stem>-extras/` next to `out`: the poster and the contact sheet.
fn extras_dir(out: &Path) -> PathBuf {
    let stem = out.file_stem().map_or_else(|| "story".into(), |s| s.to_string_lossy().into_owned());
    out.with_file_name(format!("{stem}-extras"))
}

/// Prints progress to stderr at most once a second, and on the last frame.
fn progress_printer() -> impl FnMut(&Progress) {
    let mut last: Option<Instant> = None;
    move |p: &Progress| {
        let due = last.is_none_or(|l| l.elapsed().as_secs_f64() >= 1.0) || p.frame == p.frames;
        if due {
            last = Some(Instant::now());
            eprintln!(
                "story: frame {}/{} t={:.2}s {:.1} fps eta {:.0}s waiting on {} tiles, {} missing so far",
                p.frame, p.frames, p.story_t, p.fps_recent, p.eta_s, p.waiting_on_tiles, p.missing_so_far
            );
        }
    }
}

/// Each frame's `n sha256` to stdout.
struct HashesSink;
impl FrameSink for HashesSink {
    fn push(&mut self, n: usize, f: &gpui_maps::capture::Capture) -> anyhow::Result<()> {
        println!("{n} {}", f.sha256_hex());
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({ "kind": "hashes" }))
    }
}

/// Set by SIGINT (Ctrl-C); a watcher thread copies it into the render's
/// cancel flag. A second Ctrl-C gets the default action (the handler
/// restores it), so a wedged process can still be killed from the keyboard.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(target_os = "macos")]
extern "C" fn on_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: `signal` is async-signal-safe; restoring the default action.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

/// Ctrl-C cancels the render (spec §4: the player stops before its next
/// frame, or within 50 ms of a tile wait, and the sink aborts: ffmpeg is
/// killed and its `.partial` file removed). macOS only; elsewhere Ctrl-C
/// kills the process and may leave a `.partial` file.
fn install_ctrl_c(cancel: &Arc<AtomicBool>) {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: installs a handler that only stores to an atomic.
        unsafe {
            libc::signal(libc::SIGINT, on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        let cancel = Arc::clone(cancel);
        std::thread::spawn(move || loop {
            if INTERRUPTED.load(Ordering::SeqCst) {
                eprintln!("story: interrupted; cancelling (Ctrl-C again to kill)");
                cancel.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cancel;
}

/// The exit code for `err`: 2 for a usage mistake, 130 for a cancelled
/// render, else 1.
fn error_code(err: &anyhow::Error, cancel: &AtomicBool) -> i32 {
    if err.downcast_ref::<Usage>().is_some() {
        2
    } else if cancel.load(Ordering::SeqCst) {
        130
    } else {
        1
    }
}

fn main() {
    // Mapbox is off by default: without `GPUI_MAP_ALLOW_MAPBOX=1` the token
    // is removed from the environment before the story loader can read it
    // (see `kit/src/kit.rs::MAPBOX_ALLOW_VAR`, the same switch), so
    // a `"source": "mapbox"` story fails with its "needs a token" error.
    if std::env::var("GPUI_MAP_ALLOW_MAPBOX").ok().as_deref().map(str::trim) != Some("1") {
        std::env::remove_var("MAPBOX_ACCESS_TOKEN");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    install_ctrl_c(&cancel);
    let code = match parse_args(std::env::args().skip(1)).and_then(|args| run(&args, &cancel)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("story: {e:#}");
            error_code(&e, &cancel)
        }
    };
    std::process::exit(code);
}

fn print_diagnostic(d: &story::Diagnostic) {
    let hint = d.hint.as_deref().map(|h| format!(" -- {h}")).unwrap_or_default();
    eprintln!("story: {:?} {} ({}): {}{hint}", d.severity, d.path, d.code, d.message);
}

/// The render options: the script's `output.strict` unless `--strict`
/// (which can only turn strictness on).
fn render_options(args: &Args, script: &story::Script, cancel: &Arc<AtomicBool>) -> RenderOptions {
    RenderOptions {
        strict: args.strict || script.output.strict == Some(true),
        cancel: Arc::clone(cancel),
        ..RenderOptions::default()
    }
}

/// Where the output goes: `--out`, else `~/Movies/gpui-map/<title>-<utc>.<ext>`.
fn output_path(args: &Args, script: &story::Script, script_path: &Path, ext: &str) -> anyhow::Result<PathBuf> {
    if let Some(o) = &args.out {
        return Ok(o.clone());
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| anyhow::anyhow!("HOME is unset; pass --out"))?;
    let name = script
        .title
        .as_deref()
        .map(slug)
        .filter(|s| !s.is_empty())
        .or_else(|| script_path.file_stem().map(|s| slug(&s.to_string_lossy())))
        .unwrap_or_else(|| "story".into());
    let mut file = format!("{name}-{}", utc_stamp());
    if !ext.is_empty() {
        file = format!("{file}.{ext}");
    }
    Ok(home.join("Movies/gpui-map").join(file))
}

fn run(args: &Args, cancel: &Arc<AtomicBool>) -> anyhow::Result<i32> {
    if args.help {
        println!("{USAGE}");
        return Ok(0);
    }
    if args.bench {
        return bench(args);
    }
    if args.gate {
        return gate(args);
    }
    let script_path = args.script.clone().ok_or_else(|| usage("no script named"))?;
    let raw = std::fs::read(&script_path).map_err(|e| anyhow::anyhow!("reading {}: {e}", script_path.display()))?;
    let base_dir = story_dir(&script_path)?;
    let args = &with_keyless_fallback(args, &raw, &base_dir);
    let raw = transform(&raw, args, &base_dir)?;
    let script = match story::script::parse_with_warnings(&raw) {
        Ok((script, warnings)) => {
            warnings.iter().for_each(print_diagnostic);
            script
        }
        Err(errors) => {
            if args.validate_only {
                let report = serde_json::json!({ "ok": false, "diagnostics": errors, "estimate": null, "ffmpeg": null });
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                errors.iter().for_each(print_diagnostic);
            }
            return Ok(2);
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    // One image cache for validation and the render, so an image validated
    // (and cached) once is found offline at render time.
    let image_cache_dir = args.cache.as_ref().map(|dir| dir.join("story-images"));
    let vopts = story::ValidateOptions { offline: args.offline, base_dir: base_dir.clone(), image_cache_dir: image_cache_dir.clone(), estimate: true, ..story::ValidateOptions::default() };
    let report = rt.block_on(story::validate_scene(&script, &raw, &vopts));
    if args.validate_only {
        let mut printed = serde_json::to_value(&report)?;
        // The publish table a render would carry (spec §6), judged before
        // it: every image the script names, video or not by the same rule
        // the render uses (`story::output_is_video` of the format).
        let video = story::output_is_video(&output_format(args));
        let rows = {
            let _entered = rt.enter();
            story::publish_preview(&script, &base_dir, video)
        };
        printed["publishable"] = story::burn_in::publishable(&rows).into();
        printed["publish"] = serde_json::to_value(&rows)?;
        println!("{}", serde_json::to_string_pretty(&printed)?);
        return Ok(if report.ok { 0 } else { 2 });
    }
    report.diagnostics.iter().filter(|d| d.severity != story::Severity::Info).for_each(print_diagnostic);
    if !report.ok {
        return Ok(2);
    }
    if let Some(e) = &report.estimate {
        eprintln!("story: {} frames over {:.1} s, {} tiles (~{} MB), at most {} per frame", e.frames, e.duration_s, e.tiles, e.bytes / 1_000_000, e.max_cover);
        for t in &e.tiers {
            eprintln!(
                "story:   {:>6} {}x{}: ~{} render, {} tiles, video <= {:.1} MB, GPU ~{} MB",
                t.tier.name(),
                t.width,
                t.height,
                t.wall_s_est.map_or_else(|| "?".to_string(), |s| format!("{s:.0} s")),
                t.tiles,
                t.bytes as f64 / 1e6,
                t.gpu_bytes / 1_000_000
            );
        }
    }

    let format = output_format(args);
    let codec = codec_of(args.codec.as_deref(), &script)?;
    let bundle = bundle_dir(args, &format);
    let ext = match format.as_str() {
        "mp4" if codec == sink::Codec::Vp9 => "webm",
        "mp4" => "mp4",
        "png-seq" | "hashes" => "",
        "contact-sheet" | "still" => "png",
        "png" if bundle.is_some() => "",
        "png" => return Err(usage("--format png writes a bundle without a video: give --bundle DIR")),
        other => return Err(usage(format!("--format takes mp4, png-seq, contact-sheet, still, hashes or png, got `{other}`"))),
    };
    if bundle.is_some() && !matches!(format.as_str(), "mp4" | "png") {
        return Err(usage(format!("--bundle takes --format mp4 (the default) or png, got `{format}`")));
    }
    let stem = bundle_stem(&script, &script_path);
    // Refused before any GPU work or directory exists.
    let out = match &bundle {
        Some(dir) => (format == "mp4").then(|| dir.join(format!("{stem}.{ext}"))),
        None if format == "hashes" => None,
        None => Some(output_path(args, &script, &script_path, ext)?),
    };
    if let Some(dir) = &bundle {
        story::burn_in::refuse_repo_output(dir, args.allow_repo_output)?;
    }
    if let Some(out) = &out {
        story::burn_in::refuse_repo_output(out, args.allow_repo_output)?;
    }
    let output = script.output();

    // A `--frames` range re-rendered beside an earlier render (its
    // manifest at this output's manifest path) draws the sprite sheet that
    // render recorded, so the spliced frames' icons match (review F12).
    let pinned_sprites: Option<String> = match (&args.frames, &out) {
        (Some(_), Some(out)) => std::fs::read(manifest_path(out))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|m| m.pointer("/sprites/sha256").and_then(serde_json::Value::as_str).map(str::to_string)),
        _ => None,
    };
    let mut inputs = story::scene_inputs_with_sprites(&script, &base_dir, args.offline, pinned_sprites.as_deref())?;
    // Validated above: an image gone by now is a gap, not a refusal.
    inputs.images_validated = true;
    if args.system_fonts {
        inputs.label_fonts = None;
    }
    let mut opts = render_options(args, &script, cancel);
    opts.image_cache_dir = image_cache_dir;
    opts.video = story::output_is_video(&format);
    if let Some(dir) = &args.cache {
        let safe: String = inputs.source_label.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
        opts.cache_dir = dir.join(safe);
    }
    let fps = f64::from(output.fps.max(1));
    let _entered = rt.enter();
    let mut player = Player::new(&script, &raw, inputs, opts)?;
    let frames = player.frames();
    let at_frame = |s: f64| ((s * fps).round().max(0.0) as usize).min(frames.saturating_sub(1));
    let range = match (&args.frames, args.at) {
        (Some(r), _) => r.clone(),
        (None, Some(s)) => at_frame(s)..at_frame(s) + 1,
        (None, None) if format == "still" => frames / 2..frames / 2 + 1,
        (None, None) => 0..frames,
    };
    anyhow::ensure!(range.start < range.end && range.end <= frames, "frames {}..{} are outside the story's 0..{frames}", range.start, range.end);
    player.set_frames(Some(range.clone()));

    let written = ManifestOptions { story_dir: base_dir.clone(), keyless: args.keyless };
    if let Some(dir) = &bundle {
        let opts = story::BundleOptions { dir: dir.clone(), stem, video: out.is_some(), allow_repo_output: args.allow_repo_output };
        return run_bundle(&mut player, out.as_deref(), &opts, &output, codec, report.ffmpeg.as_ref(), args.allow_missing, &written);
    }

    let Some(out) = out else {
        let mut sink = HashesSink;
        let manifest = player.run(&mut sink, &mut progress_printer())?;
        return Ok(missing_code(&manifest.frames, args.allow_missing));
    };
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let extras = extras_dir(&out);
    std::fs::create_dir_all(&extras)?;

    let result = match format.as_str() {
        "mp4" => (|| {
            let choice = sink::FfmpegChoice::Detect;
            let mut main = sink::video_sink(&choice, &out, output.width, output.height, output.fps, codec)?;
            let mut m = player.run_with_extras(&mut *main, &extras, &mut progress_printer())?;
            m.ffmpeg = report.ffmpeg.as_ref().map(story::FfmpegInfo::manifest_value);
            Ok(m)
        })(),
        "png-seq" => {
            let mut main = sink::PngSequenceSink { dir: out.clone() };
            player.run_with_extras(&mut main, &extras, &mut progress_printer())
        }
        "still" => {
            let mut main = sink::StillSink::new(out.clone());
            player.run_with_extras(&mut main, &extras, &mut progress_printer())
        }
        _ => player.run_contact_sheet(range, &out, &extras, &mut progress_printer()),
    };
    let manifest = match result {
        Ok(m) => m,
        Err(e) => {
            // The extras are written only after a successful render: an
            // empty extras directory is all a failed one leaves.
            let _ = std::fs::remove_dir(&extras);
            return Err(e);
        }
    };
    let path = manifest_path(&out);
    write_manifest(&manifest, &path, &written)?;
    let code = missing_code(&manifest.frames, args.allow_missing);
    let main = manifest.sink.get("main").unwrap_or(&manifest.sink);
    if main.get("kind").and_then(|k| k.as_str()) == Some("png_sequence_encode_later") {
        eprintln!(
            "story: no ffmpeg, so no video: wrote the frames to {} and {}; run it to make {}",
            main.pointer("/frames/dir").and_then(|d| d.as_str()).unwrap_or("?"),
            main.get("encode_sh").and_then(|d| d.as_str()).unwrap_or("?"),
            out.display()
        );
    } else {
        eprintln!("story: wrote {}", out.display());
    }
    println!("{}", path.display());
    Ok(code)
}

/// `--format`, or its default: `still` with `--at`, else `mp4`.
fn output_format(args: &Args) -> String {
    args.format.clone().unwrap_or_else(|| if args.at.is_some() { "still".into() } else { "mp4".into() })
}

/// The bundle folder: `--bundle DIR`, or `--format mp4 --out DIR/` (a
/// trailing separator, or a folder that already exists).
fn bundle_dir(args: &Args, format: &str) -> Option<PathBuf> {
    if let Some(dir) = &args.bundle {
        return Some(dir.clone());
    }
    let out = args.out.as_ref()?;
    let as_dir = out.as_os_str().to_string_lossy().ends_with(std::path::MAIN_SEPARATOR) || out.is_dir();
    (format == "mp4" && as_dir).then(|| out.clone())
}

/// The bundle's file stem: the title's slug, else the script file's, else `story`.
fn bundle_stem(script: &story::Script, script_path: &Path) -> String {
    script
        .title
        .as_deref()
        .map(slug)
        .filter(|s| !s.is_empty())
        .or_else(|| script_path.file_stem().map(|s| slug(&s.to_string_lossy())).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "story".into())
}

/// How a manifest is written: where the script's relative paths resolve
/// (every local path in the manifest is written relative to it), and
/// whether `--keyless` swapped in this repository's Liberty style.
struct ManifestOptions {
    story_dir: PathBuf,
    keyless: bool,
}

/// The name `--keyless`'s style is recorded under: the manifest never
/// holds the path to this repository's copy (controller ruling, Task 17).
const KEYLESS_STYLE_NAME: &str = "keyless:liberty";

/// `manifest` written to `path` (`story::manifest_json`: credentials
/// redacted, no absolute local path), `--keyless`'s style by name.
fn write_manifest(manifest: &RenderManifest, path: &Path, opts: &ManifestOptions) -> anyhow::Result<()> {
    let mut value = story::manifest_json(manifest, path.parent().unwrap_or(Path::new(".")), &opts.story_dir)?;
    if opts.keyless {
        if let Some(style) = value.pointer_mut("/script/scene/style") {
            *style = KEYLESS_STYLE_NAME.into();
        }
    }
    story::write_manifest_value(&value, path)
}

/// `--bundle`: the video (when `video_out`) and its manifest, then the
/// bundle (`story::write_bundle`). The manifest is written last, with the
/// bundle's poster, so it never names a poster that was not written.
#[allow(clippy::too_many_arguments)]
fn run_bundle(
    player: &mut Player,
    video_out: Option<&Path>,
    opts: &story::BundleOptions,
    output: &story::script::ResolvedOutput,
    codec: sink::Codec,
    ffmpeg: Option<&story::FfmpegInfo>,
    allow_missing: bool,
    written: &ManifestOptions,
) -> anyhow::Result<i32> {
    std::fs::create_dir_all(&opts.dir)?;
    let mut manifest = match video_out {
        Some(out) => {
            let mut main = sink::video_sink(&sink::FfmpegChoice::Detect, out, output.width, output.height, output.fps, codec)?;
            let mut m = player.run(&mut *main, &mut progress_printer())?;
            m.ffmpeg = ffmpeg.map(story::FfmpegInfo::manifest_value);
            Some(m)
        }
        None => None,
    };
    let (chapters, cues, images) = (player.chapters().to_vec(), player.caption_track(), player.images().to_vec());
    let bundle = story::write_bundle(player, &chapters, &cues, &images, opts)?;
    for f in &bundle.files {
        eprintln!("story: wrote {}", f.display());
    }
    let Some(m) = manifest.as_mut() else {
        println!("{}", opts.dir.display());
        return Ok(0);
    };
    bundle.record_in(m);
    // The manifest lives in the bundle: every bundle path in it is relative
    // to the bundle's folder, so the folder can move as a whole.
    let path = opts.dir.join(format!("{}.story.json", opts.stem));
    write_manifest(m, &path, written)?;
    if let Some(out) = video_out {
        eprintln!("story: wrote {}", out.display());
    }
    println!("{}", path.display());
    Ok(missing_code(&m.frames, allow_missing))
}

/// 3 when any frame lacks a tile (or drew none) and missing tiles were
/// not allowed, else 0.
fn missing_code(rows: &[story::FrameRow], allow: bool) -> i32 {
    let bad: Vec<usize> = rows.iter().filter(|r| !r.missing.is_empty() || !r.tiles_missing.is_empty()).map(|r| r.n).collect();
    let empty = rows.iter().filter(|r| r.tiles_drawn == 0).count();
    if empty > 0 {
        eprintln!("story: {empty} frames drew no tile");
    }
    if bad.is_empty() && empty == 0 {
        return 0;
    }
    eprintln!("story: {} frames are missing tiles (first: {:?})", bad.len(), bad.first());
    if allow { 0 } else { 3 }
}

/// p50 and p95 of `v` (nearest rank).
fn percentiles(mut v: Vec<f64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[(((v.len() as f64) * q).ceil() as usize).clamp(1, v.len()) - 1];
    (at(0.50), at(0.95))
}

/// One frame's render time: the sum of its stage times (`story::STAGES`).
fn frame_ms(r: &story::FrameRow) -> f64 {
    r.stage_ms.iter().sum()
}

/// One gated frame's counters, from its [`gpui_maps::capture::Capture`].
#[derive(Clone, Copy, Debug)]
struct GateFrame {
    retessellations: usize,
    reshapes: usize,
    layout_us: f64,
}

/// A sink that keeps each frame's [`GateFrame`] and refuses a frame that
/// drew no tile (a gate figure from a blank frame is no figure).
struct GateSink(Vec<GateFrame>);
impl FrameSink for GateSink {
    fn push(&mut self, n: usize, f: &gpui_maps::capture::Capture) -> anyhow::Result<()> {
        anyhow::ensure!(f.tiles_drawn > 0, "gate: frame {n} drew no tile");
        self.0.push(GateFrame { retessellations: f.story_retessellations, reshapes: f.story_reshapes, layout_us: f.chrome_layout_us });
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

/// The gate's themed run (spec §14.3): `raw` (the det story) with
/// `design: {}`, a `role: "route"` line with no paint across the fixture's
/// Marina Bay view -- themed into its ahead, casing and core sublayers --
/// drawn from 0.2 s to 1.8 s with a head, and the camera an orbit at the
/// opening zoom. With `card`, an event card (headline and two stats)
/// over the draw.
fn gate_script(raw: &[u8], card: bool) -> anyhow::Result<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(raw).map_err(|e| anyhow::anyhow!("the script is not JSON: {e}"))?;
    v["design"] = serde_json::json!({});
    v["scene"]["overlays"] = serde_json::json!([{
        "id": "gate-route", "line_metrics": true,
        "data": {"type": "FeatureCollection", "features": [{"type": "Feature", "properties": {},
            "geometry": {"type": "LineString", "coordinates": [[103.846, 1.2935], [103.851, 1.2880], [103.858, 1.2862]]}}]},
        "layers": [{"id": "gate-route-line", "type": "line", "role": "route"}]
    }]);
    // The camera: the script's opening jump, then a 2 s orbit. An orbit
    // moves every frame but keeps the zoom, so the rows count what a draw
    // costs and nothing else: crossing an integer zoom re-resolves a
    // layer's paint and re-tessellates it once by design (`capture::
    // story_overlays`), and det's own fly (z14.2 -> z15) would add that 1.
    if let Some(jump) = v["camera"].as_array().and_then(|c| c.first()).cloned() {
        v["camera"] = serde_json::json!([jump, {"type": "orbit", "degrees": 30, "duration": 2.0}]);
    }
    let mut events = vec![serde_json::json!({"at": 0.2, "until": 1.8, "overlay": {"layer": "gate-route-line", "effect": "draw",
        "easing": "linear", "head": {"radius_px": 8, "color": "#ffffff"}}})];
    if card {
        events.push(serde_json::json!({"at": 0.0, "until": 2.0, "card": {"headline": "Marina Bay",
            "stats": [{"value": "1.6", "unit": "km", "label": "Waterfront walk"}, {"value": "3", "label": "Bridges crossed"}]}}));
    }
    match v["events"].as_array_mut() {
        Some(e) => e.extend(events),
        None => v["events"] = serde_json::json!(events),
    }
    Ok(serde_json::to_vec(&v)?)
}

/// `--gate`: the perf gate's `story` section (spec §8.9, §14.3). The
/// script (default [`GATE_STORY`]) on the fixture tile, offline, to a null
/// sink:
///
/// - `story.uhd.frame_ms_p50`: the p50 frame time at 16:9 4K (pixel ratio 2);
/// - `story.draw.frame_ms_p50` and `story.draw.retessellations_after_first`:
///   the themed run ([`gate_script`]) at 16:9 1080p. A draw is a trim
///   uniform, so every frame after the first must re-tessellate nothing;
/// - `story.card.frame_ms_p50`, `story.card.reshapes_after_first` and
///   `story.card.layout_us_p50`: the themed run with its event card at
///   9:16 1080p -- the frame time, the label shapes of a key shaped
///   before (0: the card's text is shaped once), and the chrome layout's
///   time with a warm shape cache (frames after the first).
///
/// Plus `story.<run>.frames` (validity context, never graded). A run with
/// no frame prints no row: it is an error.
fn gate(args: &Args) -> anyhow::Result<i32> {
    let script_path = args.script.clone().unwrap_or_else(|| GATE_STORY.into());
    let raw = std::fs::read(&script_path).map_err(|e| anyhow::anyhow!("reading {}: {e}", script_path.display()))?;
    let base_dir = story_dir(&script_path)?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let _entered = rt.enter();
    let render = |raw: &[u8], aspect: story::tiers::Aspect, tier: story::tiers::Tier| -> anyhow::Result<(RenderManifest, Vec<GateFrame>)> {
        let run_args = Args { aspect: Some(aspect), tier: Some(tier), fixture: true, ..Args::default() };
        let raw = transform(raw, &run_args, &base_dir)?;
        let script = story::parse(&raw).map_err(|d| anyhow::anyhow!("the script does not parse: {d:?}"))?;
        let inputs = story::scene_inputs(&script, &base_dir, true)?;
        let mut player = Player::new(&script, &raw, inputs, RenderOptions::default())?;
        let mut sink = GateSink(Vec::new());
        let m = player.run(&mut sink, &mut |_| {})?;
        anyhow::ensure!(!m.frames.is_empty() && !sink.0.is_empty(), "gate: the run rendered no frame");
        Ok((m, sink.0))
    };
    let p50 = |m: &RenderManifest| percentiles(m.frames.iter().map(frame_ms).collect()).0;
    let (landscape, portrait) = (story::tiers::Aspect::Landscape, story::tiers::Aspect::Portrait);

    let (uhd, _) = render(&raw, landscape, story::tiers::Tier::Uhd)?;
    println!("gate: story.uhd.frame_ms_p50 {:.3} ms", p50(&uhd));
    println!("gate: story.uhd.frames {} count", uhd.frames.len());

    let (draw, counts) = render(&gate_script(&raw, false)?, landscape, story::tiers::Tier::Fullhd)?;
    let after_first: usize = counts.iter().skip(1).map(|f| f.retessellations).sum();
    println!("gate: story.draw.frame_ms_p50 {:.3} ms", p50(&draw));
    println!("gate: story.draw.retessellations_after_first {after_first} count");
    println!("gate: story.draw.frames {} count", draw.frames.len());

    let (card, counts) = render(&gate_script(&raw, true)?, portrait, story::tiers::Tier::Fullhd)?;
    anyhow::ensure!(counts.len() > 1, "gate: the card run needs frames after its first");
    let reshapes: usize = counts.iter().skip(1).map(|f| f.reshapes).sum();
    let layout_us = percentiles(counts.iter().skip(1).map(|f| f.layout_us).collect()).0;
    println!("gate: story.card.frame_ms_p50 {:.3} ms", p50(&card));
    println!("gate: story.card.reshapes_after_first {reshapes} count");
    println!("gate: story.card.layout_us_p50 {layout_us:.1} us");
    println!("gate: story.card.frames {} count", card.frames.len());
    Ok(0)
}

/// `--bench`: the script at 1080p30 to a null sink, then to ffmpeg.
fn bench(args: &Args) -> anyhow::Result<i32> {
    let script_path = args.script.clone().unwrap_or_else(|| BENCH_STORY.into());
    let raw = std::fs::read(&script_path).map_err(|e| anyhow::anyhow!("reading {}: {e}", script_path.display()))?;
    let base_dir = story_dir(&script_path)?;
    let bench_args = Args { preset: Some("1080p30".into()), keyless: args.keyless, fixture: args.fixture, ..Args::default() };
    let bench_args = with_keyless_fallback(&bench_args, &raw, &base_dir);
    let raw = transform(&raw, &bench_args, &base_dir)?;
    let script = story::parse(&raw).map_err(|d| anyhow::anyhow!("the script does not parse: {d:?}"))?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let _entered = rt.enter();
    let output = script.output();

    let mut runs: Vec<(&str, f64, RenderManifest)> = Vec::new();
    {
        let inputs = story::scene_inputs(&script, &base_dir, args.offline)?;
        let label = inputs.source_label.clone();
        let mut player = Player::new(&script, &raw, inputs, RenderOptions::default())?;
        eprintln!("bench: {} ({label}) {}x{} at {} fps, {} frames", script_path.display(), output.width, output.height, output.fps, player.frames());
        let started = Instant::now();
        let m = player.run(&mut sink::NullSink, &mut progress_printer())?;
        runs.push(("null sink", started.elapsed().as_secs_f64(), m));
    }
    match sink::detect_ffmpeg() {
        Some(info) => {
            let dir = std::env::temp_dir().join(format!("gpui-map-bench-{}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            let out = dir.join("bench.mp4");
            let inputs = story::scene_inputs(&script, &base_dir, args.offline)?;
            let mut player = Player::new(&script, &raw, inputs, RenderOptions::default())?;
            let mut main = sink::FfmpegSink::start(&info, &out, output.width, output.height, output.fps, sink::Codec::H264)?;
            let started = Instant::now();
            let m = player.run(&mut main, &mut progress_printer())?;
            runs.push(("ffmpeg", started.elapsed().as_secs_f64(), m));
            let _ = std::fs::remove_dir_all(&dir);
        }
        None => eprintln!("bench: no ffmpeg; the ffmpeg run is skipped"),
    }

    println!("bench: {} {}x{}@{}", script_path.display(), output.width, output.height, output.fps);
    for (name, wall, m) in &runs {
        let n = m.frames.len();
        let missing = m.frames.iter().filter(|r| !r.missing.is_empty() || !r.tiles_missing.is_empty()).count();
        println!("  {name:<9} {n} frames in {wall:.2} s = {:.1} fps (frames missing tiles: {missing})", n as f64 / wall.max(1e-9));
        let split: Vec<String> = story::STAGES
            .iter()
            .enumerate()
            .map(|(i, stage)| {
                let (p50, p95) = percentiles(m.frames.iter().map(|r| r.stage_ms.get(i).copied().unwrap_or(0.0)).collect());
                format!("{stage} {p50:.2}/{p95:.2}")
            })
            .collect();
        println!("            p50/p95 ms: {}", split.join(", "));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(v: &[&str]) -> anyhow::Result<Args> {
        parse_args(v.iter().map(|s| (*s).to_string()))
    }

    fn not_cancelled() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn keyless_maps_mapbox_layers_to_liberty_and_repeats_events() {
        let mut v = serde_json::json!({
            "scene": {"style": "examples/mapbox-streets-style.json", "basemap": {"source": "mapbox"}},
            "events": [
                {"at": 1, "set_layer_visibility": {"layer": "poi-label", "visible": false}},
                {"at": 2, "set_paint_property": {"layer": "water", "name": "fill-color", "value": "#000"}},
                {"at": 3, "caption": {"text": "x", "anchor": "top-left"}},
                {"at": 4, "set_layer_visibility": {"layer": "settlement-major-label", "visible": false}}
            ]
        });
        let unmapped = keyless(&mut v, &["water".to_string(), "poi_r1".to_string()], "../liberty-style.json");
        assert_eq!(v["scene"]["style"], "../liberty-style.json");
        assert_eq!(v["scene"]["basemap"]["source"], "openfreemap");
        let events = v["events"].as_array().unwrap();
        assert_eq!(events.len(), 6);
        assert_eq!(events[0]["set_layer_visibility"]["layer"], "poi_r20");
        assert_eq!(events[3]["set_paint_property"]["layer"], "water");
        assert_eq!(unmapped, vec!["settlement-major-label".to_string()], "the unmapped Mapbox id is reported");
    }

    /// Review fix round 1, Finding 1 (out-of-diff root cause): a script
    /// with a static `scene.credits` naming Mapbox must not still credit
    /// Mapbox once `--keyless` has swapped its basemap to OpenFreeMap --
    /// the controller's own poster.png showed both credited at once
    /// (`Player::verdict` chains the live source's own stated credit onto
    /// the script's static one with no basemap-consistency check). A
    /// non-Mapbox credit in the same list survives untouched.
    #[test]
    fn keyless_drops_mapbox_from_a_static_scene_credits_list() {
        let mut v = serde_json::json!({
            "scene": {
                "style": "examples/mapbox-streets-style.json",
                "basemap": {"source": "mapbox"},
                "credits": [
                    "Mapbox Streets v8, Terrain v2, Bathymetry v2 — Mapbox, OpenStreetMap contributors",
                    "Building footprints — OpenFreeMap, OpenStreetMap contributors"
                ]
            },
            "events": []
        });
        keyless(&mut v, &[], "../liberty-style.json");
        let credits = v["scene"]["credits"].as_array().expect("credits stays an array");
        assert_eq!(credits.len(), 1, "{credits:?}");
        assert!(credits[0].as_str().unwrap().contains("OpenFreeMap"), "{credits:?}");
        assert!(!credits.iter().any(|c| c.as_str().is_some_and(|s| s.contains("Mapbox"))), "{credits:?}");
    }

    /// A script with no `scene.credits` at all (every shipped
    /// `examples/stories/*.json`) must not gain one -- `keyless` only ever
    /// filters an existing list.
    #[test]
    fn keyless_adds_no_credits_field_when_the_script_has_none() {
        let mut v = serde_json::json!({
            "scene": {"style": "examples/mapbox-streets-style.json", "basemap": {"source": "mapbox"}},
            "events": []
        });
        keyless(&mut v, &[], "../liberty-style.json");
        assert!(v["scene"].get("credits").is_none(), "{v}");
    }

    /// A Mapbox story renders keyless by itself when it has no token or
    /// its Mapbox style is absent (the open library tree), and as written
    /// otherwise; a non-Mapbox story is never touched.
    #[test]
    fn a_mapbox_story_falls_back_keyless_without_a_token_or_its_style() {
        let tmp = std::env::temp_dir().join(format!("story-keyless-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let script = |style: &str, source: &str| format!(r#"{{"scene":{{"style":"{style}","basemap":{{"source":"{source}"}}}}}}"#).into_bytes();
        let mapbox = script("mapbox-streets-style.json", "mapbox");
        // The style is absent: keyless, token or not, fixture or not.
        assert!(keyless_fallback(&mapbox, &tmp, Some("pk.x"), false).is_some_and(|w| w.contains("not in this checkout")));
        assert!(keyless_fallback(&mapbox, &tmp, Some("pk.x"), true).is_some());
        std::fs::write(tmp.join("mapbox-streets-style.json"), b"{}").unwrap();
        // Present: keyless only without a token, and never for a fixture run.
        assert!(keyless_fallback(&mapbox, &tmp, None, false).is_some_and(|w| w.contains("no Mapbox token")));
        assert!(keyless_fallback(&mapbox, &tmp, Some("  "), false).is_some());
        assert_eq!(keyless_fallback(&mapbox, &tmp, Some("pk.x"), false), None);
        assert_eq!(keyless_fallback(&mapbox, &tmp, None, true), None);
        assert_eq!(keyless_fallback(&script("liberty-style.json", "openfreemap"), &tmp, None, false), None);
        // `--keyless` already given: nothing to add.
        let given = args_of(&["x.json", "--keyless"]).unwrap();
        assert!(with_keyless_fallback(&given, &mapbox, &tmp).keyless);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The three example stories load and validate as their keyless twins
    /// (no token here), and -- controller ruling, Task 18 -- the
    /// layers-over-time tween moves something the frame shows: a paint
    /// tween of the water, not `raster_opacity` on a scene with no raster.
    #[test]
    fn the_example_stories_validate_and_layers_over_time_tweens_something_visible() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let keyless = args_of(&["x.json", "--keyless"]).unwrap();
        for name in ["sg-to-tokyo", "orbit-marina", "layers-over-time"] {
            let path = PathBuf::from(format!("examples/stories/{name}.json"));
            // `sg-to-tokyo` stays with Studio (scripts/split-manifest.txt);
            // the open library tree has the other two.
            if name == "sg-to-tokyo" && !path.is_file() {
                eprintln!("skipped: {} is not in this tree", path.display());
                continue;
            }
            let dir = story_dir(&path).unwrap();
            let raw = transform(&std::fs::read(&path).unwrap(), &keyless, &dir).unwrap();
            let script = story::parse(&raw).unwrap_or_else(|d| panic!("{name}: {d:?}"));
            let report = rt.block_on(story::validate_scene(&script, &raw, &story::ValidateOptions { offline: true, base_dir: dir, image_cache_dir: None, estimate: true, ..story::ValidateOptions::default() }));
            let errors: Vec<_> = report.diagnostics.iter().filter(|d| d.severity == story::Severity::Error).collect();
            assert!(report.ok, "{name}: {errors:?}");
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read("examples/stories/layers-over-time.json").unwrap()).unwrap();
        let events = v["events"].as_array().unwrap();
        let has_raster = v["scene"].get("raster").is_some();
        assert!(
            !events.iter().any(|e| e["tween"]["param"] == "raster_opacity") || has_raster,
            "a raster_opacity tween on a scene with no raster animates nothing"
        );
        assert!(events.iter().any(|e| e["tween_paint_property"]["layer"] == "water"), "the water tween");
    }

    #[test]
    fn slugs_and_stamps() {
        assert_eq!(slug("Singapore to Tokyo (OpenFreeMap)"), "singapore-to-tokyo-openfreemap");
        assert_eq!(stamp_of(0), "19700101T000000Z");
        assert_eq!(stamp_of(951_782_400), "20000229T000000Z", "a leap day");
        assert_eq!(stamp_of(1_774_000_000), "20260320T094640Z");
        assert_eq!(utc_stamp().len(), 16);
    }

    #[test]
    fn usage_mistakes_exit_2() {
        let cancel = not_cancelled();
        for bad in [vec!["x.json", "--nope"], vec!["x.json", "--frames", "..5"], vec!["x.json", "--frames", "0..5", "--at", "1"], vec!["a.json", "b.json"]] {
            let e = args_of(&bad).err().unwrap_or_else(|| panic!("{bad:?} parsed"));
            assert_eq!(error_code(&e, &cancel), 2, "{bad:?}: {e:#}");
        }
        let e = args_of(&["x.json", "--frames", "..5"]).err().unwrap();
        assert!(format!("{e}").contains("--frames"), "{e}");
    }

    /// Task R3: `--safe-area all` (spec §5.1, the union of the three
    /// portrait apps) parses and reaches `output.safe_area`, and a bad
    /// value's message names it among the choices.
    #[test]
    fn safe_area_all_parses_and_is_offered() {
        let a = args_of(&["x.json", "--safe-area", "all"]).unwrap();
        assert_eq!(a.safe_area, Some(story::tiers::SafeArea::All));
        let raw = br#"{"story":1,"output":{"preset":"vertical1080"},"scene":{"style":"s.json","basemap":{"source":"fixture"}},
            "camera":[{"type":"jump","to":{"lon":0,"lat":0,"zoom":2}},{"type":"hold","duration":1}]}"#;
        let v: serde_json::Value = serde_json::from_slice(&transform(raw, &a, Path::new(".")).unwrap()).unwrap();
        assert_eq!(v["output"]["safe_area"].as_str(), Some("all"));
        let e = args_of(&["x.json", "--safe-area", "vine"]).err().unwrap();
        assert!(format!("{e}").contains("shorts, all,"), "{e}");
    }

    /// Task 18 (F17): one story renders at 9:16 1080p and at 16:9 4K by
    /// flags alone. The flag not given keeps the script's own, the rate
    /// is kept, and a size mixed with a tier is a usage mistake.
    #[test]
    fn aspect_tier_and_safe_area_flags_override_the_scripts_output() {
        let cancel = not_cancelled();
        let raw = br#"{"story":1,"output":{"preset":"vertical1080","fps":24,"strict":true},"scene":{"style":"s.json","basemap":{"source":"fixture"}},
            "camera":[{"type":"jump","to":{"lon":0,"lat":0,"zoom":2}},{"type":"hold","duration":1}]}"#;
        let dir = Path::new(".");
        let out = |flags: &[&str]| -> serde_json::Value {
            let mut argv = vec!["x.json"];
            argv.extend_from_slice(flags);
            serde_json::from_slice(&transform(raw, &args_of(&argv).unwrap(), dir).unwrap()).unwrap()
        };
        let v = out(&["--tier", "4k"]);
        assert_eq!(v["output"], serde_json::json!({"aspect":"9:16","tier":"4k","fps":24,"strict":true}), "vertical1080 stays 9:16");
        let s = story::parse(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!((s.output().width, s.output().height, s.output().fps), (2160, 3840, 24));
        let v = out(&["--aspect", "16:9", "--tier", "4k", "--safe-area", "youtube"]);
        assert_eq!((v["output"]["aspect"].as_str(), v["output"]["tier"].as_str(), v["output"]["safe_area"].as_str()), (Some("16:9"), Some("4k"), Some("youtube")));
        assert!(v["output"].get("preset").is_none());
        let v = out(&["--aspect", "1:1"]);
        assert_eq!((v["output"]["aspect"].as_str(), v["output"]["tier"].as_str()), (Some("1:1"), Some("fullhd")), "the tier kept");
        for bad in [vec!["x.json", "--tier", "8k"], vec!["x.json", "--aspect", "2:1"], vec!["x.json", "--safe-area", "vine"], vec!["x.json", "--preset", "4k30", "--tier", "4k"], vec!["x.json", "--codec", "prores"]] {
            match args_of(&bad) {
                Err(e) => assert_eq!(error_code(&e, &cancel), 2, "{bad:?}: {e:#}"),
                // `--codec` is judged against the script, after parsing.
                Ok(a) => {
                    let s = story::parse(raw).unwrap();
                    let e = codec_of(a.codec.as_deref(), &s).unwrap_err();
                    assert_eq!(error_code(&e, &cancel), 2, "{bad:?}: {e:#}");
                }
            }
        }
        let s = story::parse(raw).unwrap();
        assert_eq!(codec_of(Some("hevc"), &s).unwrap(), sink::Codec::Hevc);
        assert_eq!(codec_of(None, &s).unwrap(), sink::Codec::H264);
    }

    #[test]
    fn the_scripts_strict_is_honoured_and_the_flag_only_adds() {
        let script_with = |strict: Option<bool>| {
            let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read("tests/fixtures/stories/det.json").unwrap()).unwrap();
            if let Some(s) = strict {
                v["output"]["strict"] = s.into();
            }
            story::parse(&serde_json::to_vec(&v).unwrap()).unwrap()
        };
        let cancel = not_cancelled();
        let plain = args_of(&["x.json"]).unwrap();
        let flag = args_of(&["x.json", "--strict"]).unwrap();
        assert!(!render_options(&plain, &script_with(None), &cancel).strict);
        assert!(render_options(&plain, &script_with(Some(true)), &cancel).strict, "the script's strict");
        assert!(render_options(&flag, &script_with(None), &cancel).strict, "the flag");
        assert!(render_options(&flag, &script_with(Some(false)), &cancel).strict, "the flag over a false");
    }

    #[test]
    fn validate_only_prints_the_report_and_exits_2_on_a_bad_layer() {
        let dir = tempfile::tempdir().unwrap();
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read("tests/fixtures/stories/det.json").unwrap()).unwrap();
        v["events"][0]["set_layer_visibility"]["layer"] = "no-such-layer".into();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        let args = args_of(&[path.to_str().unwrap(), "--offline", "--validate-only"]).unwrap();
        assert_eq!(run(&args, &not_cancelled()).unwrap(), 2);
        let good = args_of(&["tests/fixtures/stories/det.json", "--offline", "--validate-only"]).unwrap();
        assert_eq!(run(&good, &not_cancelled()).unwrap(), 0);
    }

    #[test]
    fn an_output_inside_the_repository_is_refused_before_anything_is_written() {
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("story-refusal-test.png");
        let args = args_of(&["tests/fixtures/stories/det.json", "--offline", "--fixture", "--format", "still", "--out", out.to_str().unwrap()]).unwrap();
        let cancel = not_cancelled();
        let e = run(&args, &cancel).unwrap_err();
        assert!(format!("{e:#}").contains("--allow-repo-output"), "{e:#}");
        assert_eq!(error_code(&e, &cancel), 1);
        assert!(!out.exists() && !extras_dir(&out).exists(), "nothing written");
    }

    #[test]
    fn a_still_renders_and_exits_0_with_its_manifest_and_extras() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("still.png");
        let args = args_of(&["tests/fixtures/stories/det.json", "--offline", "--fixture", "--at", "1.0", "--out", out.to_str().unwrap()]).unwrap();
        assert_eq!(run(&args, &not_cancelled()).unwrap(), 0);
        assert!(out.is_file() && manifest_path(&out).is_file());
        assert!(extras_dir(&out).join("poster.png").is_file() && extras_dir(&out).join("contact.png").is_file());
    }

    /// `--bundle DIR --format png`: the bundle with no video and no
    /// manifest; every file the spec lists, the implicit chapter's still,
    /// and the poster equal to it.
    #[test]
    fn a_png_bundle_writes_the_bundle_without_a_video() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("b");
        let args = args_of(&["tests/fixtures/stories/det.json", "--offline", "--fixture", "--format", "png", "--bundle", out.to_str().unwrap()]).unwrap();
        assert_eq!(run(&args, &not_cancelled()).unwrap(), 0);
        let still = out.join("stills/story.png"); // the implicit chapter
        for f in ["det.srt", "det.vtt", "transcript.html", "poster.png"] {
            assert!(out.join(f).is_file(), "{f}");
        }
        assert!(still.is_file(), "{still:?}");
        assert_eq!(std::fs::read(out.join("poster.png")).unwrap(), std::fs::read(&still).unwrap());
        assert!(!out.join("det.mp4").exists() && !out.join("det.story.json").exists());
        assert!(std::fs::read_to_string(out.join("det.vtt")).unwrap().contains("Marina Bay"));
    }

    /// The child half of the next test: renders the story the parent
    /// names, as a bundle, from whatever current directory the parent
    /// started it in. Ignored (and a no-op) unless run as that child.
    #[test]
    #[ignore = "run only as the child process of a_story_renders_from_any_current_directory"]
    fn cwd_child() {
        let (Some(script), Some(out)) = (std::env::var_os("STORY_CWD_CHILD_SCRIPT"), std::env::var_os("STORY_CWD_CHILD_OUT")) else { return };
        let (script, out) = (script.to_string_lossy().into_owned(), out.to_string_lossy().into_owned());
        let args = args_of(&[&script, "--offline", "--fixture", "--bundle", &out]).unwrap();
        assert_eq!(run(&args, &not_cancelled()).unwrap(), 0);
    }

    /// Controller ruling, fix round 1: a script's relative style and image
    /// paths resolve against the script file's folder, not the current
    /// directory. The story folder holds its own style and image; the
    /// render runs in a child process whose current directory is another,
    /// empty folder -- no symlinks. The bundle's manifest names its own
    /// files relative to the bundle (finding 4).
    #[test]
    fn a_story_renders_from_any_current_directory() {
        let root = tempfile::tempdir().unwrap();
        let (story_dir, elsewhere, out) = (root.path().join("story"), root.path().join("elsewhere"), root.path().join("out"));
        std::fs::create_dir_all(story_dir.join("img")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::copy(LIBERTY_STYLE, story_dir.join("liberty.json")).unwrap();
        image::RgbaImage::from_fn(60, 40, |x, y| image::Rgba([(x * 4) as u8, (y * 6) as u8, 128, 255])).save(story_dir.join("img/a.png")).unwrap();
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read("tests/fixtures/stories/det.json").unwrap()).unwrap();
        v["title"] = "From elsewhere".into();
        v["scene"]["style"] = "liberty.json".into();
        v["events"].as_array_mut().unwrap().push(serde_json::json!(
            {"at": 0.4, "until": 1.6, "image": {"src": "img/a.png", "alt": "A gradient", "credit": "Photo: Test", "placement": "corner"}}));
        let script = story_dir.join("story.json");
        std::fs::write(&script, serde_json::to_vec(&v).unwrap()).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["tests::cwd_child", "--exact", "--ignored", "--nocapture"])
            .current_dir(&elsewhere)
            .env("STORY_CWD_CHILD_SCRIPT", &script)
            .env("STORY_CWD_CHILD_OUT", &out)
            .status()
            .unwrap();
        assert!(status.success(), "the child render failed: {status}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0, "nothing written in the current directory");
        let media: Vec<_> = std::fs::read_dir(out.join("media")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(media.len(), 1);
        assert_eq!(std::fs::read(&media[0]).unwrap(), std::fs::read(story_dir.join("img/a.png")).unwrap(), "the story folder's image");
        for f in ["poster.png", "stills/story.png", "from-elsewhere.srt", "from-elsewhere.vtt", "transcript.html"] {
            assert!(out.join(f).is_file(), "{f}");
        }
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(out.join("from-elsewhere.story.json")).unwrap()).unwrap();
        assert_eq!(manifest["poster"], "poster.png", "relative to the manifest's folder");
        let text = serde_json::to_string(&manifest["sink"]).unwrap();
        assert!(!text.contains(&*out.to_string_lossy()) && !text.contains(&*out.canonicalize().unwrap().to_string_lossy()), "{text}");
        assert!(manifest.pointer("/sink/out").is_none_or(|o| o == "from-elsewhere.mp4"), "{text}");
        // Task 17: the chapter's still and the publish table, and no
        // absolute local path anywhere (controller privacy ruling).
        assert_eq!(manifest["chapters"][0][1], "stills/story.png");
        assert_eq!(manifest["coverage"]["captions"], 1);
        assert!(manifest["publish"].as_array().unwrap().iter().any(|r| r["source"] == "image:img/a.png" && r["verdict"] == "ok"));
        assert_no_local_path(&out.join("from-elsewhere.story.json"), &[root.path()]);
    }

    /// No byte of `manifest` names the home directory, `/Users/`,
    /// `/home/`, or `dirs` (as given or canonical).
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
            assert!(!text.contains(b.as_str()), "{} records `{b}`:\n{text}", manifest.display());
        }
        // No string is a local absolute path at all (fix round 1: not even
        // a system ffmpeg's); `/events/2`-style JSON pointers are not paths.
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
        let local = |s: &str| {
            s.starts_with('~')
                || ["/Users", "/home", "/Volumes", "/private", "/tmp", "/var", "/opt", "/usr", "/nix"].iter().any(|r| s.starts_with(r))
        };
        let absolute: Vec<&&str> = all.iter().filter(|s| local(s)).collect();
        assert!(absolute.is_empty(), "{} records local paths: {absolute:?}", manifest.display());
    }

    /// Controller ruling (Task 17): `--keyless`'s style is this
    /// repository's file; the manifest names it `keyless:liberty`, never
    /// its path, and records no absolute local path at all.
    #[test]
    fn a_keyless_manifest_names_its_style_and_no_local_path() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("story/det.json");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::copy("tests/fixtures/stories/det.json", &script).unwrap();
        let out = root.path().join("out/det.png");
        let args =
            args_of(&[script.to_str().unwrap(), "--offline", "--keyless", "--fixture", "--at", "0.5", "--out", out.to_str().unwrap()]).unwrap();
        assert_eq!(run(&args, &not_cancelled()).unwrap(), 0);
        let path = manifest_path(&out);
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(manifest["script"]["scene"]["style"], KEYLESS_STYLE_NAME);
        assert_eq!(manifest["publishable"], true, "{}", manifest["publish"]);
        assert_no_local_path(&path, &[root.path()]);
    }

    #[test]
    fn bundle_usage_mistakes_exit_2() {
        let cancel = not_cancelled();
        for bad in [vec!["x.json", "--bundle", "d", "--at", "1"], vec!["x.json", "--bundle", "d", "--frames", "0..2"]] {
            let e = args_of(&bad).err().unwrap_or_else(|| panic!("{bad:?} parsed"));
            assert_eq!(error_code(&e, &cancel), 2, "{bad:?}: {e:#}");
        }
        let dir = tempfile::tempdir().unwrap();
        for bad in [vec!["--format", "png"], vec!["--format", "still", "--bundle", dir.path().to_str().unwrap()]] {
            let mut argv = vec!["tests/fixtures/stories/det.json", "--offline", "--fixture"];
            argv.extend(bad.iter().copied());
            let e = run(&args_of(&argv).unwrap(), &cancel).unwrap_err();
            assert_eq!(error_code(&e, &cancel), 2, "{bad:?}: {e:#}");
        }
        assert_eq!(bundle_dir(&args_of(&["x.json", "--out", "some/dir/"]).unwrap(), "mp4"), Some(PathBuf::from("some/dir/")));
        assert_eq!(bundle_dir(&args_of(&["x.json", "--out", "some/file.mp4"]).unwrap(), "mp4"), None);
    }

    #[test]
    fn a_cancelled_render_exits_130_and_leaves_no_video() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("c.mp4");
        let args = args_of(&["tests/fixtures/stories/det.json", "--offline", "--fixture", "--out", out.to_str().unwrap()]).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let e = run(&args, &cancel).unwrap_err();
        assert!(format!("{e:#}").contains("cancelled"), "{e:#}");
        assert_eq!(error_code(&e, &cancel), 130);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "no video, no .partial, no extras: {left:?}");
    }

    #[test]
    fn missing_tiles_exit_3_unless_allowed() {
        let row = |n: usize, missing: bool| story::FrameRow {
            n,
            t: 0.0,
            lon: 0.0,
            lat: 0.0,
            zoom: 0.0,
            pitch_deg: 0.0,
            bearing_deg: 0.0,
            globe: false,
            ms: 0.0,
            wait_ms: 0.0,
            tiles_drawn: 4,
            labels_drawn: 0,
            overlay_labels_drawn: 0,
            icons_drawn: 0,
            line_glyphs_drawn: 0,
            line_glyphs_dropped: 0,
            missing: if missing { vec![("1/2/3".into(), story::prefetch::MissReason::Timeout)] } else { Vec::new() },
            tiles_missing: Vec::new(),
            sha256: String::new(),
            stage_ms: [0.0; 7],
            layout_warnings: Vec::new(),
        };
        assert_eq!(missing_code(&[row(0, false), row(1, false)], false), 0);
        assert_eq!(missing_code(&[row(0, false), row(1, true)], false), 3);
        assert_eq!(missing_code(&[row(0, false), row(1, true)], true), 0);
    }
}

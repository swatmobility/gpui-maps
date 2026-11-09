//! Every tracked satellite over the globe (spec
//! `docs/superpowers/specs/2026-09-29-satellites-design.md`): the
//! satellites panel ([`panel::SatellitesPanel`]) in a window, its headless
//! captures, and the CelesTrak fetch policy's one-shot flags.
//!
//! ```text
//! cargo run --release --features orbits --example satellites -- [--cache DIR] [--no-fetch]
//!     [--synthetic | --omm FILE] [--at UTC] [--rate R] [--basemap mapbox|positron]
//!     [--size WxH] [--pixel-ratio R] [--capture DIR] [--time-load]
//!     [--fetch-only GROUP] [--seed-stations FILE --seed-fetched-unix T]
//!     [--measure] [--measure-frames SECS [--hide-all]] [--measure-look]
//! ```
//!
//! - `--cache DIR`: the cache directory, default
//!   `~/.cache/gpui-map-data/satellites` (never inside the repository).
//! - `--no-fetch`: draw the cache as it is; never ask CelesTrak. Without
//!   it the window refreshes each enabled group once on open, under the two
//!   hour rule (`satellites/fetch.rs`).
//! - `--synthetic`: the seeded synthetic set (no data, no network).
//! - `--omm FILE`: one OMM JSON file, classified by the cache's group lists.
//! - `--at UTC` (`2026-09-29T05:25:57Z`): the scene time to open at; now
//!   by default. `--rate R`: scene seconds per wall second (0 paused,
//!   negative backwards).
//! - `--basemap`: Mapbox Light (needs `MAPBOX_ACCESS_TOKEN`) or the keyless
//!   OpenFreeMap Positron; by default Mapbox Light with a token.
//! - `--capture DIR`: no window. Five headless PNGs over the chosen data at
//!   the scene time (`--size`, default 1280x800): `shells_z-1`,
//!   `iss_selected`, `geo`, `follow_z5` and `rate_1000` (the scene 10,000 s
//!   on). Never fetches.
//! - `--time-load`: no window. Times the cold load (read, parse, catalogue,
//!   one keyframe of every object) and exits. Never fetches.
//! - `--seed-stations FILE --seed-fetched-unix T`: records an already
//!   fetched `stations` body as fetched at Unix time `T`, without a request.
//! - `--fetch-only GROUP`: one [`fetch::Fetcher::refresh`] of `GROUP` (one
//!   of [`fetch::GROUPS`]) under the two hour policy -- at most one request,
//!   none when it is not due -- then a summary of the cached body.
//! - `--measure` (with `--synthetic`): no window. The `sats` perf-gate
//!   section (`docs/perf-gate.md`): keyframe, propagation, hit-test and
//!   parse times, and the exact visible counts and upload size, as
//!   `gate: sats.* VALUE UNIT` lines. Never fetches.
//! - `--measure-look`: no window. The `look` perf-gate section: what the
//!   space look (`render::space_look`, on by default in this scene) costs
//!   -- GPU ms/frame of its two passes at a 2x MacBook and a 5K surface,
//!   the first-use pipeline build with a full-size texture upload, and the
//!   per-frame uniform write -- as `gate: look.* VALUE UNIT` lines. Seeded
//!   synthetic textures at the real assets' sizes; never fetches.
//! - `--measure-frames SECS` (`--features probe`): the windowed `sats.run`
//!   section. Opens 1440x900 at lat 20, z -1, at `--measure`'s scene time
//!   unless `--at` (keyless Positron unless
//!   `--basemap` says otherwise) with two rings drawn, warms up 2 s, then
//!   prints `gate: sats.run.frame_p95_ms` and `gate: sats.run.draw_calls`
//!   over SECS s and quits. `--hide-all` starts with every group's chip off
//!   and draws no rings: the A/B control.
//!
//! Data: GP element sets from the US Space Force, via CelesTrak
//! (<https://celestrak.org>); see `demos/README.md` for the terms.

// The panel, its model, About text and fetch (`demos/satellites/*.rs`),
// through the gallery panel that includes them: ONE copy, shared with the
// gallery's `satellites` panel.
use swat_maps_kit::panels::satellites as satellites_panel;
#[cfg(not(target_arch = "wasm32"))]
use satellites_panel::fetch;
use satellites_panel::{model, panel};

// The demo keys every example window has: `t` (terminator), `g` (globe).
use swat_maps_kit::shell;

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    native::main();
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use gpui::{px, size, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions};
    use gpui_maps::capture::{capture, fetch_cover, AltitudeCapture, CaptureRequest};
    use gpui_maps::orbit::catalog::{Catalog, GROUP_COUNT, ISS_NORAD};
    use gpui_maps::orbit::propagate;
    use gpui_maps::capture::CaptureKeyframes;
    use gpui_maps::render::altitude::{AltitudeLine, GeoPoint, KeyState, KeyframeSource, RefFrame};
    use gpui_maps::render::altitude_pipeline::AltitudeLook;

    use super::{fetch, model, panel};
    use panel::{Basemap, DataSource, PanelOptions, SatellitesPanel};

    struct Args {
        cache: Option<PathBuf>,
        no_fetch: bool,
        synthetic: bool,
        omm: Option<PathBuf>,
        at: Option<f64>,
        rate: f64,
        basemap: Option<Basemap>,
        size: (u32, u32),
        /// `--pixel-ratio R`: `--capture`'s physical pixels per logical pixel (default 1).
        pixel_ratio: f32,
        capture: Option<PathBuf>,
        time_load: bool,
        fetch_only: Option<String>,
        seed: Option<PathBuf>,
        seed_t: Option<f64>,
        measure: bool,
        measure_look: bool,
        measure_frames: Option<f64>,
        hide_all: bool,
        look: AltitudeLook,
        trails: bool,
        earth: Option<model::Earth>,
        /// `--only a,b`: capture just these presets.
        only: Option<Vec<String>>,
    }

    /// `2026-09-29T05:25:57Z` (or with a space, or fractional seconds) as
    /// Unix seconds.
    pub(super) fn parse_utc(s: &str) -> Option<f64> {
        let s = s.trim().trim_end_matches('Z').replace(' ', "T");
        let t = chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f").ok()?.and_utc();
        Some(t.timestamp() as f64 + f64::from(t.timestamp_subsec_nanos()) * 1e-9)
    }

    fn parse_args() -> Args {
        let mut a = Args {
            cache: None,
            no_fetch: false,
            synthetic: false,
            omm: None,
            at: None,
            rate: 1.0,
            basemap: None,
            size: (1280, 800),
            pixel_ratio: 1.0,
            capture: None,
            time_load: false,
            fetch_only: None,
            seed: None,
            seed_t: None,
            measure: false,
            measure_look: false,
            measure_frames: None,
            hide_all: false,
            look: AltitudeLook::Glow,
            trails: true,
            earth: None,
            only: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut value = || it.next().unwrap_or_else(|| usage(&format!("{arg} needs a value")));
            match arg.as_str() {
                "--cache" => a.cache = Some(PathBuf::from(value())),
                "--no-fetch" => a.no_fetch = true,
                "--synthetic" => a.synthetic = true,
                "--omm" => a.omm = Some(PathBuf::from(value())),
                "--at" => a.at = Some(parse_utc(&value()).unwrap_or_else(|| usage("--at takes UTC, e.g. 2026-09-29T05:25:57Z"))),
                "--rate" => a.rate = value().parse().unwrap_or_else(|_| usage("--rate takes a number")),
                "--basemap" => {
                    a.basemap = Some(match value().as_str() {
                        "mapbox" => Basemap::MapboxLight,
                        "positron" => Basemap::Positron,
                        other => usage(&format!("--basemap takes mapbox|positron, got {other}")),
                    });
                }
                "--size" => {
                    let v = value();
                    a.size = v
                        .split_once('x')
                        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                        .unwrap_or_else(|| usage(&format!("--size takes WxH, got {v}")));
                }
                "--pixel-ratio" => {
                    let v = value();
                    a.pixel_ratio = v.parse().ok().filter(|r: &f32| *r > 0.0).unwrap_or_else(|| usage(&format!("--pixel-ratio takes a positive number, got {v}")));
                }
                "--capture" => a.capture = Some(PathBuf::from(value())),
                "--time-load" => a.time_load = true,
                "--fetch-only" => a.fetch_only = Some(value()),
                "--seed-stations" => a.seed = Some(PathBuf::from(value())),
                "--seed-fetched-unix" => {
                    a.seed_t = Some(value().parse::<f64>().unwrap_or_else(|_| usage("--seed-fetched-unix takes Unix seconds")));
                }
                "--measure" => a.measure = true,
                "--measure-look" => a.measure_look = true,
                "--measure-frames" => {
                    a.measure_frames = Some(value().parse::<f64>().ok().filter(|s| *s > 0.0).unwrap_or_else(|| usage("--measure-frames takes seconds")));
                }
                "--hide-all" => a.hide_all = true,
                "--look" => {
                    a.look = match value().as_str() {
                        "glow" => AltitudeLook::Glow,
                        "flat" => AltitudeLook::Flat,
                        other => usage(&format!("--look takes glow|flat, got {other}")),
                    }
                }
                "--no-trails" => a.trails = false,
                "--only" => a.only = Some(value().split(',').map(str::to_string).collect()),
                "--earth" => {
                    a.earth = Some(match value().as_str() {
                        "real" => model::Earth::Real,
                        "streets" => model::Earth::Streets,
                        "map" => model::Earth::Map,
                        "night" => model::Earth::Night,
                        other => usage(&format!("--earth takes real|streets|map|night, got {other}")),
                    })
                }
                "-h" | "--help" => usage(""),
                other => usage(&format!("unknown argument {other}")),
            }
        }
        if a.synthetic && a.omm.is_some() {
            usage("--synthetic and --omm are exclusive");
        }
        if a.measure && !a.synthetic {
            usage("--measure runs on the seeded set only: add --synthetic");
        }
        if a.hide_all && a.measure_frames.is_none() {
            usage("--hide-all is --measure-frames' control run");
        }
        if a.measure_frames.is_some() && !cfg!(feature = "probe") {
            usage("--measure-frames needs --features probe (the frame log)");
        }
        if a.basemap == Some(Basemap::MapboxLight) && panel::mapbox_token().is_none() {
            usage("--basemap mapbox needs MAPBOX_ACCESS_TOKEN");
        }
        a
    }

    fn data_source(args: &Args, cache: &Path, fetch_allowed: bool) -> DataSource {
        if args.synthetic {
            DataSource::Synthetic
        } else if let Some(file) = &args.omm {
            DataSource::Omm { file: file.clone(), groups_dir: Some(cache.to_path_buf()) }
        } else {
            DataSource::Cache { dir: cache.to_path_buf(), fetch: fetch_allowed && !args.no_fetch }
        }
    }

    fn now() -> f64 {
        panel::wall_unix()
    }

    pub fn main() {
        let args = parse_args();
        let cache = args.cache.clone().unwrap_or_else(fetch::default_cache_dir);
        let fetcher = fetch::Fetcher::celestrak(cache.clone());

        let one_shot = args.seed.is_some() || args.seed_t.is_some() || args.fetch_only.is_some();
        match (&args.seed, args.seed_t) {
            (Some(file), Some(t)) => {
                let body = std::fs::read(file).unwrap_or_else(|e| usage(&format!("{}: {e}", file.display())));
                fetcher.seed("stations", &body, t).unwrap_or_else(|e| usage(&format!("seed: {e}")));
                let meta = fetcher.meta("stations");
                println!("seeded stations in {}: {meta:?}", cache.display());
                println!("{}", fetch::status_line("stations", &meta, now()));
            }
            (None, None) => {}
            _ => usage("--seed-stations and --seed-fetched-unix go together"),
        }
        if let Some(group) = &args.fetch_only {
            if !fetch::GROUPS.contains(&group.as_str()) {
                usage(&format!("--fetch-only takes one of {:?}", fetch::GROUPS));
            }
            let started = Instant::now();
            // The WALL clock: the policy's two hours are real hours.
            let outcome = fetcher.refresh(group, now());
            let took = started.elapsed();
            let meta = fetcher.meta(group);
            println!("{group}: {outcome:?} in {:.0} ms", took.as_secs_f64() * 1e3);
            println!("meta: {meta:?}");
            println!("{}", fetch::status_line(group, &meta, now()));
            summarise(&fetcher, group);
        }
        if one_shot {
            return;
        }
        if args.measure_look {
            super::measure::look();
        } else if args.measure {
            super::measure::run();
        } else if args.time_load {
            time_load(&data_source(&args, &cache, false));
        } else if let Some(dir) = &args.capture {
            capture_presets(&args, &data_source(&args, &cache, false), dir);
        } else {
            windowed(&args, data_source(&args, &cache, true));
        }
    }

    /// The window.
    fn windowed(args: &Args, data: DataSource) {
        println!("Drag to pan, scroll to zoom. Hover a dot for its card; click to select; the time bar runs the scene.");
        println!("Keys: {} day/night terminator, {} globe or flat, Esc deselects.", shell::TERMINATOR_KEY.to_uppercase(), shell::GLOBE_KEY.to_uppercase());
        // A measured run draws the keyless basemap unless told otherwise, so
        // a token in the environment cannot change what the gate times.
        let basemap = args.basemap.unwrap_or_else(|| {
            if args.measure_frames.is_some() {
                Basemap::Positron
            } else {
                Basemap::for_token(panel::mapbox_token().as_deref())
            }
        });
        println!("basemap: {}; data: {data:?}", basemap.name());
        let (w, h) = if args.measure_frames.is_some() { (1440.0, 900.0) } else { (args.size.0 as f32, args.size.1 as f32) };
        let mut options = PanelOptions { basemap, data, scene_unix: args.at, rate: args.rate, size: (w, h), look: args.look, trails: args.trails, earth: args.earth, ..PanelOptions::default() };
        if args.measure_frames.is_some() {
            // The headless section's camera and scene time, unless `--at`.
            options.at = MEASURE_AT;
            options.scene_unix = Some(args.at.unwrap_or(super::measure::SCENE_UNIX));
        }
        if args.hide_all {
            options.groups = [false; GROUP_COUNT];
        }
        let measure = args.measure_frames.map(|secs| (secs, args.hide_all));
        let mut options = Some(options);
        gpui_platform::application().run(move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
            let handle = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions { title: Some("Satellites".into()), ..Default::default() }),
                    ..Default::default()
                },
                |window, cx| {
                    let options = options.take().expect("one window");
                    cx.new(|cx| SatellitesPanel::with_options(window, cx, options))
                },
            )
            .expect("failed to open window");
            // Frontmost, or the window server never asks it to paint.
            cx.activate(true);
            #[cfg(feature = "probe")]
            if let Some((secs, hide_all)) = measure {
                let map = handle.update(cx, |panel, _, _| panel.map().clone()).expect("the panel");
                measure_frames(map, secs, hide_all, cx);
            }
            #[cfg(not(feature = "probe"))]
            let _ = (measure, handle);
        });
    }

    /// `--measure-frames`' camera: lon 0, lat 20, z -1 (the hit test's).
    const MEASURE_AT: (f64, f64, f64) = (0.0, 20.0, -1.0);
    /// The rings `--measure-frames` draws (spec §8's "10k points plus 2
    /// rings"): the first object and the first GEO one, under ids the panel
    /// never uses.
    #[cfg(feature = "probe")]
    const MEASURE_RINGS: [&str; 2] = ["sat-measure-ring-0", "sat-measure-ring-1"];

    /// The windowed `sats.run` section: 2 s of warm-up, then SECS s of the
    /// frame log; frame time is paint-to-paint (what reaches the screen,
    /// GPU back-pressure included). Quits when done.
    #[cfg(feature = "probe")]
    fn measure_frames(map: gpui::Entity<gpui_maps::map_view::MapView>, secs: f64, hide_all: bool, cx: &mut App) {
        use gpui_maps::render::{allocation_stats, frame_log_start, frame_log_take};
        use std::time::Duration;
        cx.spawn(async move |cx| {
            if !hide_all {
                let catalog = model::synthetic_loaded().catalog;
                let geo = catalog.objects.iter().position(|o| o.group == gpui_maps::orbit::catalog::Group::Geo).unwrap_or(0);
                let scene = map.read_with(cx, |m, _| m.scene_clock().map(|c| c.at(Instant::now()))).unwrap_or_else(now);
                map.update(cx, |m, cx| {
                    for (id, i) in MEASURE_RINGS.into_iter().zip([0, geo]) {
                        let obj = &catalog.objects[i];
                        let line = AltitudeLine { points: propagate::orbit_ring_teme(obj, model::ring_start(obj, scene), panel::RING_SAMPLES), color: model::group_rgba(obj.group, 0.7), closed: true };
                        m.set_altitude_lines(id, RefFrame::Inertial, 1.5, vec![line], cx);
                    }
                });
            }
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let draws0 = allocation_stats().draw_calls_total;
            frame_log_start();
            cx.background_executor().timer(Duration::from_secs_f64(secs)).await;
            let paints = frame_log_take();
            let draws = allocation_stats().draw_calls_total - draws0;
            let mut gaps: Vec<f64> = paints.windows(2).map(|w| w[1].ended_at.duration_since(w[0].ended_at).as_secs_f64() * 1e3).collect();
            let mut cpu: Vec<f64> = paints.iter().map(|p| p.paint_nanos as f64 / 1e6).collect();
            gaps.sort_by(f64::total_cmp);
            cpu.sort_by(f64::total_cmp);
            let q = |v: &[f64], p: f64| if v.is_empty() { 0.0 } else { v[((v.len() - 1) as f64 * p).round() as usize] };
            println!(
                "sats.run: {} paints in {secs:.1} s ({:.1} fps){} | frame p50 {:.2} p95 {:.2} max {:.2} ms | paint cpu p50 {:.2} p95 {:.2} ms | draw calls {draws}{}",
                paints.len(),
                paints.len() as f64 / secs,
                if hide_all { " [hide-all]" } else { "" },
                q(&gaps, 0.5),
                q(&gaps, 0.95),
                q(&gaps, 1.0),
                q(&cpu, 0.5),
                q(&cpu, 0.95),
                if draws == 0 { " (NOTHING PAINTED: void)" } else { "" }
            );
            println!("gate: sats.run.frame_p95_ms {:.3} ms", q(&gaps, 0.95));
            println!("gate: sats.run.draw_calls {draws} count");
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    /// `--time-load`: the cold load, headless: read + parse + catalogue
    /// (what the panel runs off the UI thread before the layer attaches),
    /// then one keyframe of every object (what the map's worker computes
    /// before the first dots are drawn; it computes two).
    fn time_load(data: &DataSource) {
        let t = Instant::now();
        let result = panel::load(data);
        let load_ms = t.elapsed().as_secs_f64() * 1e3;
        let Some(loaded) = result.loaded else {
            println!("no catalogue for {data:?} (read in {load_ms:.0} ms)");
            return;
        };
        let n = loaded.catalog.objects.len();
        let source = propagate::CatalogSource(loaded.catalog.clone());
        let mut out = vec![gpui_maps::render::altitude::KeyState::HIDDEN; n];
        let t = Instant::now();
        gpui_maps::render::altitude::KeyframeSource::states_at(&source, now(), 0..n, &mut out);
        let key_ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "time-load: {n} objects {:?} [stations, starlink, gps, geo, other, galileo, glonass, beidou]; read + parse + catalogue {load_ms:.0} ms; one keyframe of all {key_ms:.1} ms; to the first two keyframes ~{:.0} ms",
            loaded.catalog.group_counts(),
            load_ms + 2.0 * key_ms
        );
    }

    /// One `--capture` frame.
    struct Preset {
        name: &'static str,
        /// `None`: centred on the ISS's sub-point.
        centre: Option<(f64, f64)>,
        /// `None`: `fit_shell_zoom(GEO)`.
        zoom: Option<f64>,
        /// Seconds after the scene time.
        dt: f64,
        /// The ISS drawn as selected (1.5 px ring at 70%) rather than by
        /// default (1 px at 50%).
        selected: bool,
        /// The groups drawn, as the chips would show them.
        groups: [bool; GROUP_COUNT],
        /// Drawn from a keyframe pair with the clock at this rate, so the
        /// style's trails show (0: static points, no trails).
        rate: f64,
        /// `Some`: coloured by owner country, these classes shown.
        countries: Option<[bool; model::COUNTRY_COUNT]>,
        /// The orbit-altitude preset.
        alt: model::AltPreset,
        /// Coloured by type even with country filters (the crossfilter:
        /// colour-by is a palette, independent of what is filtered).
        by_type: bool,
    }

    const fn preset(name: &'static str, centre: Option<(f64, f64)>, zoom: Option<f64>, dt: f64, selected: bool) -> Preset {
        Preset { name, centre, zoom, dt, selected, groups: [true; GROUP_COUNT], rate: 0.0, countries: None, alt: model::AltPreset::All, by_type: false }
    }

    const PRESETS: [Preset; 17] = [
        // The crossfilter: China only, coloured by type; every type but
        // Starlink, LEO only.
        Preset { countries: Some([false, true, false, false, false, false, false, false]), by_type: true, ..preset("xf_china_only", Some((105.0, 30.0)), Some(0.3), 0.0, false) },
        Preset { groups: [true, false, true, true, true, true, true, true], alt: model::AltPreset::Leo, ..preset("xf_no_starlink_leo", Some((10.0, 20.0)), Some(1.0), 0.0, false) },
        Preset { countries: Some([true; model::COUNTRY_COUNT]), ..preset("country_shells", Some((10.0, 20.0)), Some(-0.3), 0.0, false) },
        Preset { countries: Some([false, false, false, true, false, false, false, false]), ..preset("eu_only", Some((10.0, 20.0)), Some(-0.3), 0.0, false) },
        Preset { countries: Some([false, true, false, false, false, false, false, false]), ..preset("china_only", Some((10.0, 20.0)), Some(-0.3), 0.0, false) },
        Preset { alt: model::AltPreset::Meo, ..preset("meo_only", Some((10.0, 20.0)), Some(-0.1), 0.0, false) },
        Preset { alt: model::AltPreset::Geo, ..preset("geo_only", Some((10.0, 0.0)), None, 0.0, false) },
        preset("shells_z-1", Some((10.0, 20.0)), Some(-1.0), 0.0, false),
        preset("shells_z-0.3", Some((10.0, 20.0)), Some(-0.3), 0.0, false),
        Preset { groups: [false, true, false, false, false, false, false, false], ..preset("starlink_only", Some((10.0, 20.0)), Some(-0.3), 0.0, false) },
        // The navigation family's MEO shells (~19-23k km): GPS, Galileo,
        // GLONASS, BeiDou only.
        Preset { groups: [false, false, true, false, false, true, true, true], ..preset("meo_navigation", Some((10.0, 20.0)), Some(-0.1), 0.0, false) },
        preset("iss_selected", None, Some(1.2), 0.0, true),
        preset("geo", Some((10.0, 0.0)), None, 0.0, false),
        preset("follow_z5", None, Some(5.0), 0.0, true),
        preset("rate_1000", None, Some(1.2), 10_000.0, false),
        Preset { rate: 1000.0, ..preset("trails_shells_1000x", Some((10.0, 20.0)), Some(-0.3), 0.0, false) },
        Preset { rate: 1000.0, ..preset("trails_iss_1000x", None, Some(1.2), 0.0, true) },
    ];

    /// Every object's keyframe pair around `t` for a clock at `rate`: `k0`
    /// at the interval's grid point at or before `t`, `k1` one interval on
    /// (the window's own `keyframe_delta_s`).
    fn keyframes_at(catalog: &std::sync::Arc<Catalog>, t: f64, rate: f64) -> CaptureKeyframes {
        let d = gpui_maps::render::altitude::keyframe_delta_s(rate);
        let t0 = (t / d).floor() * d;
        let src = propagate::CatalogSource(std::sync::Arc::clone(catalog));
        let n = catalog.objects.len();
        let (mut k0, mut k1) = (vec![KeyState::HIDDEN; n], vec![KeyState::HIDDEN; n]);
        src.states_at(t0, 0..n, &mut k0);
        src.states_at(t0 + d, 0..n, &mut k1);
        CaptureKeyframes { frame: RefFrame::Inertial, t0_unix: t0, delta_s: d, k0, k1, scene_unix: t, rate }
    }

    /// Every object at `t` as an Earth-fixed point with its paint, as the
    /// window's keyframed layer places it (a capture has no worker): the
    /// points, their style, and the ISS's index among them.
    fn points_at(
        loaded: &model::Loaded,
        t: f64,
        look: AltitudeLook,
        filters: &model::Filters,
        by_country: bool,
        earth: model::Earth,
    ) -> (Vec<GeoPoint>, gpui_maps::render::altitude_pipeline::AltitudeStyle, Option<usize>) {
        let catalog = &loaded.catalog;
        let gmst = gpui_maps::orbit::gmst_rad(t);
        let mut style = if by_country {
            let countries: Vec<model::Country> = loaded.stats.iter().map(|s| s.country).collect();
            model::style_by_country(catalog, &countries, look, false, earth)
        } else {
            model::style_for_earth(catalog, look, false, earth)
        };
        let (mut points, mut paint, mut iss) = (Vec::new(), Vec::new(), None);
        for (i, o) in catalog.objects.iter().enumerate().filter(|(i, _)| filters.passes(&loaded.stats[*i])) {
            let Some(state) = propagate::state_at(o, t) else { continue };
            if o.record.norad_id == ISS_NORAD {
                iss = Some(points.len());
            }
            let [x, y, z] = propagate::teme_to_ecef_km(state.position_km, gmst);
            let r = (x * x + y * y + z * z).sqrt();
            points.push(GeoPoint { lon: y.atan2(x).to_degrees(), lat: (z / r).asin().to_degrees(), alt_m: r * 1000.0 - gpui_maps::render::globe::GLOBE_RADIUS_M });
            paint.push(style.paint[i]);
        }
        style.paint = paint;
        (points, style, iss)
    }

    /// The imagery tiles of `cover`, read through `dir`; a tile that will
    /// not fetch or decode is left out (drawn as the background).
    fn raster_cover(
        runtime: &tokio::runtime::Runtime,
        source: &dyn gpui_maps::tile_source::TileSource,
        cover: &[gpui_maps::geo::TileId],
        size: u32,
        dir: &Path,
    ) -> std::collections::HashMap<gpui_maps::geo::TileId, image::RgbaImage> {
        let _ = std::fs::create_dir_all(dir);
        let mut out = std::collections::HashMap::new();
        for &t in cover {
            let path = dir.join(format!("{}-{}-{}.img", t.z, t.x, t.y));
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => match runtime.block_on(source.fetch(t)) {
                    Ok(Some(b)) => {
                        let _ = std::fs::write(&path, &b);
                        b
                    }
                    _ => continue,
                },
            };
            match gpui_maps::raster::decode_tile(&bytes, size) {
                Ok(img) => {
                    out.insert(t, img);
                }
                Err(e) => eprintln!("imagery tile {}/{}/{}: {e}", t.z, t.x, t.y),
            }
        }
        out
    }

    /// `--capture DIR`: [`PRESETS`] headless, over the chosen basemap.
    fn capture_presets(args: &Args, data: &DataSource, dir: &Path) {
        std::fs::create_dir_all(dir).expect("create the capture dir");
        let result = panel::load(data);
        let Some(loaded) = result.loaded else {
            eprintln!("satellites: no catalogue for {data:?}; try --synthetic");
            std::process::exit(2);
        };
        let catalog = loaded.catalog.clone();
        let scene = args.at.unwrap_or_else(now);
        let basemap = args.basemap.unwrap_or_else(|| Basemap::for_token(panel::mapbox_token().as_deref()));
        let earth = args.earth.unwrap_or(if args.look == AltitudeLook::Glow { model::Earth::Real } else { model::Earth::Map });
        let spec = panel::basemap_spec_earth(basemap, panel::mapbox_token().as_deref(), earth);
        let raster_cache = std::env::temp_dir().join("gpui-map-capture-raster-natural-earth");
        println!(
            "basemap: {}; {} objects {:?}; scene {} (load {:.0} ms)",
            basemap.name(),
            catalog.objects.len(),
            catalog.group_counts(),
            model::format_utc(scene),
            result.took_ms
        );
        let runtime = tokio::runtime::Runtime::new().expect("a tokio runtime");
        let cache = std::env::temp_dir().join(format!("gpui-map-capture-satellites-{}", basemap.name().replace(' ', "-").to_lowercase()));
        let iss_obj = catalog.index_of(ISS_NORAD).map(|i| &catalog.objects[i]);
        // The Distributions card's numbers for this catalogue (all filters
        // open, stacked by group), for rendering the charts off-window.
        {
            let f = model::Filters::default();
            let alt = model::histogram(&loaded.stats, &f, true, GROUP_COUNT, |s| s.group as usize);
            let incl = model::histogram(&loaded.stats, &f, false, GROUP_COUNT, |s| s.group as usize);
            let counts = model::class_counts(&loaded.stats, &f, GROUP_COUNT, |s| s.group as usize);
            let colours: Vec<String> = (0..GROUP_COUNT).map(|i| format!("#{:06x}", model::NEON_COLOURS[i])).collect();
            let names: Vec<&str> = model::GROUPS.iter().map(|&g| model::group_name(g)).collect();
            let order: Vec<usize> = model::GROUPS.iter().map(|&g| g as usize).collect();
            let json = serde_json::json!({ "alt": alt, "incl": incl, "counts": counts, "colours": colours, "names": names, "order": order });
            std::fs::write(dir.join("distributions.json"), json.to_string()).expect("write distributions.json");
        }
        for p in PRESETS.iter().filter(|p| args.only.as_ref().is_none_or(|only| only.iter().any(|n| n == p.name))) {
            let t = scene + p.dt;
            let (range, heo) = p.alt.range();
            let filters = model::Filters { groups: p.groups, countries: p.countries.unwrap_or([true; model::COUNTRY_COUNT]), alt_km: range, heo_only: heo, ..model::Filters::default() };
            let (points, style, iss) = points_at(&loaded, t, args.look, &filters, p.countries.is_some() && !p.by_type, earth);
            let iss_at = iss.map(|i| points[i]);
            let (lon, lat) = p.centre.or(iss_at.map(|q| (q.lon, q.lat))).unwrap_or((10.0, 20.0));
            let (w, h) = args.size;
            let zoom = p.zoom.unwrap_or_else(|| {
                // Fitted in logical pixels: the view a ratio-1 frame of that size shows.
                gpui_maps::camera_control::fit_shell_zoom(model::GEO_RADIUS_M, 0.1, lat, w as f32 / args.pixel_ratio, h as f32 / args.pixel_ratio).expect("a GEO fit")
            });
            let mut request = CaptureRequest { center_lon: lon, center_lat: lat, zoom, width: w, height: h, pixel_ratio: args.pixel_ratio, ..CaptureRequest::default() };
            let mut altitude = AltitudeCapture { points, style, ..Default::default() };
            altitude.emphasis = (None, None, iss);
            if p.rate != 0.0 {
                // The window's path: a keyframe pair on the GPU, trails on.
                altitude.keyframes = Some(keyframes_at(&catalog, t, p.rate));
                altitude.style = model::style_for_earth(&catalog, args.look, args.trails, earth);
                altitude.emphasis.2 = catalog.index_of(ISS_NORAD);
            }
            let iss = altitude.emphasis.2;
            if let Some(obj) = iss_obj {
                let (width, alpha) = if p.selected { (1.5, 0.7) } else { (1.0, 0.5) };
                altitude.lines = vec![AltitudeLine { points: propagate::orbit_ring_teme(obj, model::ring_start(obj, t), panel::RING_SAMPLES), color: model::scene_rgba(args.look, earth, obj.group, alpha), closed: true }];
                altitude.lines_look = model::scene_look(args.look, earth);
                altitude.lines_frame = RefFrame::Inertial;
                altitude.lines_width_px = width;
                altitude.lines_scene_unix = t;
                let colour = model::scene_hex(args.look, earth, obj.group);
                let look = model::scene_look(args.look, earth);
                // One overlay source for the whole track, which spans every
                // longitude: the engine's grid reaches it all from one origin.
                request.overlay = Some(model::track_geojson(look, &propagate::ground_track(obj, t), &colour));
                // The glowing track's three features carry their own colours.
                request.overlay_paint = (look == AltitudeLook::Flat).then(|| format!(r#"{{"line-color":"{colour}"}}"#));
            }
            if p.selected {
                altitude.emphasis.1 = iss;
            }
            request.altitude = Some(altitude);
            let cover = request.cover();
            let tiles = runtime.block_on(fetch_cover(spec.tiles.as_ref(), &cover, &cache)).expect("basemap tiles");
            if let Some((source, size)) = spec.raster.as_ref() {
                // "Earth: real": the imagery cover, read through its own
                // disk cache (Natural Earth, public domain; never CelesTrak).
                request.raster = raster_cover(&runtime, source.as_ref(), &cover, *size, &raster_cache);
                request.raster_opacity = model::REAL_EARTH_OPACITY;
            }
            let frame = capture(&request, &tiles, &spec.style).expect("a capture");
            let path = dir.join(format!("{}.png", p.name));
            frame.save_png(&path).expect("write a PNG");
            println!(
                "{}: lon {lon:.2} lat {lat:.2} z{zoom:.2} at {}; {} tiles, {} altitude points{} -> {}",
                p.name,
                model::format_utc(t),
                frame.tiles_drawn,
                frame.altitude_points_drawn,
                iss_at.map_or_else(String::new, |q| format!("; ISS at {:.2},{:.2} {:.0} km", q.lon, q.lat, q.alt_m / 1000.0)),
                path.display()
            );
        }
    }

    /// The cached body of `group`: parse report, catalogue, group counts,
    /// and the epoch spread. Group membership comes from whichever group
    /// lists are cached (never fetched here).
    fn summarise(fetcher: &fetch::Fetcher, group: &str) {
        let Some(body) = fetcher.cached(group) else {
            println!("no cached {group} body");
            return;
        };
        if group == fetch::SATCAT {
            match model::parse_satcat(&body) {
                Ok(owners) => {
                    let mut by = [0usize; model::COUNTRY_COUNT];
                    for code in owners.values() {
                        by[model::country_of_owner(code) as usize] += 1;
                    }
                    println!("satcat: {} owners; by class {by:?} [US, China, Russia, EU, Japan, India, UK, Other]", owners.len());
                }
                Err(e) => println!("satcat: does not parse: {e}"),
            }
            return;
        }
        let active = (group == "active").then_some(body.clone());
        let bodies = model::Bodies {
            active: active.or_else(|| Some(body.clone())),
            stations: fetcher.cached("stations"),
            gps: fetcher.cached("gps-ops"),
            geo: fetcher.cached("geo"),
            galileo: fetcher.cached("galileo"),
            glonass: fetcher.cached("glo-ops"),
            beidou: fetcher.cached("beidou"),
            satcat: fetcher.cached("satcat"),
        };
        match model::catalog_from_bodies(&bodies) {
            None => println!("the cached {group} body has no usable records"),
            Some(l) => {
                println!(
                    "drawn {}, skipped {} (parse + sgp4), group counts {:?} [stations, starlink, gps, geo, other, galileo, glonass, beidou]",
                    l.catalog.objects.len(),
                    l.skipped,
                    l.catalog.group_counts()
                );
                if let Some((min, median, max)) = l.catalog.epoch_stats_unix() {
                    println!("epochs: min {}, median {}, max {}", model::format_utc(min), model::format_utc(median), model::format_utc(max));
                }
            }
        }
    }

    fn usage(problem: &str) -> ! {
        if !problem.is_empty() {
            eprintln!("satellites: {problem}");
        }
        eprintln!(
            "usage: satellites [--cache DIR] [--no-fetch] [--synthetic | --omm FILE] [--at UTC] [--rate R] [--basemap mapbox|positron] [--size WxH] [--pixel-ratio R] [--capture DIR] [--time-load] [--fetch-only {}] [--seed-stations FILE --seed-fetched-unix T] [--measure] [--measure-frames SECS [--hide-all]] [--look glow|flat] [--no-trails] [--earth real|streets|map|night] [--only PRESET,...]",
            fetch::GROUPS.join("|")
        );
        std::process::exit(if problem.is_empty() { 0 } else { 2 });
    }

    use super::shell;
}

/// The `sats` perf-gate section (`examples/perf_gate.rs`, `docs/perf-gate.md`):
/// `--measure --synthetic` headless, and the windowed `--measure-frames`'
/// scene. Everything here runs on the seeded synthetic set at a fixed scene
/// time, so the counts are the same on every run and every machine.
#[cfg(not(target_arch = "wasm32"))]
mod measure {
    use std::sync::Arc;
    use std::time::Instant;

    use gpui_maps::orbit::catalog::Catalog;
    use gpui_maps::orbit::omm::{parse_omm, OmmRecord};
    use gpui_maps::orbit::propagate::CatalogSource;
    use gpui_maps::orbit::synthetic::{synthetic_membership, synthetic_records, SYNTHETIC_EPOCH_UNIX};
    use gpui_maps::render::altitude::{hidden_bounds, teme_to_sphere, KeyState, KeyframeBuilder, KEYFRAME_DELTA_MAX_S};
    use gpui_maps::render::altitude_pipeline::{dot_redraw_after_s, hit_test, keyed_instance, main_flags, AltitudeFrame, PointInstance, HIT_RADIUS_PX};
    use gpui_maps::render::Camera;

    use super::model;

    /// The scene: an hour after the synthetic epoch.
    pub const SCENE_UNIX: f64 = SYNTHETIC_EPOCH_UNIX + 3_600.0;
    /// The seed the demo's own `--synthetic` draws (`model::synthetic_loaded`).
    const SEED: u64 = 7;
    /// Every camera: 1440x900 logical, 1:1, north up, flat pitch.
    const VIEWPORT: (f32, f32) = (1440.0, 900.0);
    /// `sats.visible.<name>`: (name, zoom, lat), lon 0.
    pub const VISIBLE_CAMERAS: [(&str, f64, f64); 3] = [("z-1_lat0", -1.0, 0.0), ("z-1_lat60", -1.0, 60.0), ("z3_lat0", 3.0, 0.0)];
    /// The hit test's camera (spec §5.4, MG-S4's): z -1, lat 20.
    const HIT_CAMERA: (f64, f64) = (-1.0, 20.0);
    /// The hit test's grid.
    const GRID: (u32, u32) = (64, 48);

    /// The deterministic half of the section: no timing.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Counts {
        /// Instances drawn (not planet-hidden, not behind the near plane) at
        /// each of [`VISIBLE_CAMERAS`].
        pub visible: [usize; 3],
        /// One keyframe upload, `⌈instances x 60 / 1024⌉`.
        pub upload_kb: usize,
    }

    /// The synthetic catalogue, as `--synthetic` builds it.
    fn catalog() -> Arc<Catalog> {
        model::synthetic_loaded().catalog
    }

    /// One full keyframe of every object at `t`, single-threaded, as the
    /// native worker computes it (one step of `usize::MAX`).
    fn keyframe(source: &CatalogSource, t: f64) -> Vec<KeyState> {
        let mut b = KeyframeBuilder::new(t, source.0.objects.len());
        while !b.step(source, usize::MAX) {}
        b.finish().states
    }

    /// The instances the layer uploads at [`SCENE_UNIX`]: keyframes at the
    /// scene time and one interval on, every object shown, no emphasis.
    fn instances(catalog: &Arc<Catalog>) -> Vec<PointInstance> {
        let source = CatalogSource(catalog.clone());
        let delta = KEYFRAME_DELTA_MAX_S;
        let (k0, k1) = (keyframe(&source, SCENE_UNIX), keyframe(&source, SCENE_UNIX + delta));
        let style = model::style_for(catalog);
        (0..k0.len())
            .map(|i| {
                let hidden = hidden_bounds(&source, i, &k0[i], &k1[i], SCENE_UNIX, delta);
                keyed_instance(&k0[i], &k1[i], delta, hidden, main_flags(&style, &[], (None, None, None), i))
            })
            .collect()
    }

    /// The layer's frame at `(zoom, lat)`, lon 0, at `s = 0` of the
    /// interval starting at [`SCENE_UNIX`] (the TEME keyframes turned into
    /// the sphere frame at that instant).
    fn frame(zoom: f64, lat: f64) -> AltitudeFrame {
        let camera = Camera {
            center_lon: 0.0,
            center_lat: lat,
            zoom,
            pitch: 0.0,
            bearing: 0.0,
            viewport_width_px: VIEWPORT.0,
            viewport_height_px: VIEWPORT.1,
            fov_y: Camera::DEFAULT_FOV_Y,
        };
        let globe = gpui_maps::render::globe::globe_frame(&camera);
        AltitudeFrame::new(&camera, &globe, teme_to_sphere(SCENE_UNIX), 0.0, KEYFRAME_DELTA_MAX_S as f32, 1.0).expect("a globe frame at every gate camera")
    }

    fn counts_of(instances: &[PointInstance]) -> Counts {
        let visible = VISIBLE_CAMERAS.map(|(_, zoom, lat)| {
            let f = frame(zoom, lat);
            instances.iter().filter(|inst| f.position(inst).is_some()).count()
        });
        let upload_kb = std::mem::size_of_val(instances).div_ceil(1024);
        Counts { visible, upload_kb }
    }

    /// [`Counts`] alone (the unit test's entry point).
    #[cfg(test)]
    pub fn measure_counts() -> Counts {
        counts_of(&instances(&catalog()))
    }

    /// `records` as the CelesTrak OMM JSON body, keys in CelesTrak's order.
    fn omm_json(records: &[OmmRecord]) -> Vec<u8> {
        let s = |v: &str| serde_json::to_string(v).expect("a JSON string");
        let n = |v: f64| serde_json::to_string(&v).expect("a JSON number");
        let body: Vec<String> = records
            .iter()
            .map(|r| {
                format!(
                    r#"{{"OBJECT_NAME":{},"OBJECT_ID":{},"EPOCH":{},"MEAN_MOTION":{},"ECCENTRICITY":{},"INCLINATION":{},"RA_OF_ASC_NODE":{},"ARG_OF_PERICENTER":{},"MEAN_ANOMALY":{},"EPHEMERIS_TYPE":0,"CLASSIFICATION_TYPE":"U","NORAD_CAT_ID":{},"ELEMENT_SET_NO":{},"REV_AT_EPOCH":{},"BSTAR":{},"MEAN_MOTION_DOT":{},"MEAN_MOTION_DDOT":{}}}"#,
                    s(&r.object_name),
                    s(&r.object_id),
                    s(&r.epoch.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()),
                    n(r.mean_motion),
                    n(r.eccentricity),
                    n(r.inclination_deg),
                    n(r.raan_deg),
                    n(r.arg_pericenter_deg),
                    n(r.mean_anomaly_deg),
                    r.norad_id,
                    r.element_set_no,
                    r.rev_at_epoch,
                    n(r.bstar),
                    n(r.mean_motion_dot),
                    n(r.mean_motion_ddot),
                )
            })
            .collect();
        format!("[{}]", body.join(",")).into_bytes()
    }

    /// How long `run` propagates untimed before timing `keyframe_ms`.
    const KEYFRAME_WARM_UP: std::time::Duration = std::time::Duration::from_millis(250);

    fn best_of<T>(runs: usize, mut f: impl FnMut() -> T) -> (f64, T) {
        let mut best = f64::INFINITY;
        let mut last = None;
        for _ in 0..runs {
            let t = Instant::now();
            let out = std::hint::black_box(f());
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
            last = Some(out);
        }
        (best, last.expect("at least one run"))
    }

    fn gate(name: &str, value: f64, unit: &str) {
        println!("gate: sats.{name} {value} {unit}");
    }

    /// `--measure --synthetic`: the section's eight `gate:` lines.
    pub fn run() {
        let catalog = catalog();
        let n = catalog.objects.len();
        let source = CatalogSource(catalog.clone());

        // Untimed keyframes first, for KEYFRAME_WARM_UP of wall time, not a
        // count: this is the first work the process does, and a process
        // launched after the machine idled starts its thread slow (DVFS
        // and core placement ramp) for ~40 ms -- 1.4x, at ~200 ns against
        // ~140 ns. One warm-up keyframe plus a best of 5 (~10 ms) sat
        // wholly inside that ramp on every gate launch (the gate pauses
        // between sections), so `propagate_ns` failed at +38% on old and
        // new binaries alike while back-to-back runs passed. 250 ms is
        // 6x the longest ramp seen; a best of 20 then covers any leftover.
        let warm = Instant::now();
        while warm.elapsed() < KEYFRAME_WARM_UP {
            std::hint::black_box(keyframe(&source, SCENE_UNIX));
        }
        let (keyframe_ms, states) = best_of(20, || keyframe(&source, SCENE_UNIX));
        let valid = states.iter().filter(|s| s.valid).count();
        let propagate_ns = keyframe_ms * 1e6 / n as f64;

        let instances = instances(&catalog);
        let f = frame(HIT_CAMERA.0, HIT_CAMERA.1);
        let cells = f64::from(GRID.0 * GRID.1);
        let mut passes: Vec<f64> = (0..5)
            .map(|_| {
                let t = Instant::now();
                let mut hits = 0usize;
                for gy in 0..GRID.1 {
                    for gx in 0..GRID.0 {
                        let x = (gx as f32 + 0.5) * VIEWPORT.0 / GRID.0 as f32;
                        let y = (gy as f32 + 0.5) * VIEWPORT.1 / GRID.1 as f32;
                        hits += usize::from(hit_test(&f, &instances, x, y, HIT_RADIUS_PX).is_some());
                    }
                }
                std::hint::black_box(hits);
                t.elapsed().as_secs_f64() * 1e6 / cells
            })
            .collect();
        passes.sort_by(f64::total_cmp);
        let hit_us = passes[passes.len() / 2];

        let records = synthetic_records(SEED);
        let body = omm_json(&records);
        let membership = synthetic_membership(&records);
        let (parse_ms, parsed) = best_of(5, || {
            let report = parse_omm(&body).expect("the synthetic body parses");
            let parsed = report.records.len();
            (parsed, Catalog::build(report.records, &membership).objects.len())
        });

        let counts = counts_of(&instances);
        println!(
            "sats: {n} synthetic objects ({valid} propagated) at {}; OMM body {} KB -> {} records -> {} objects",
            model::format_utc(SCENE_UNIX),
            body.len() / 1024,
            parsed.0,
            parsed.1
        );
        gate("keyframe_ms", round(keyframe_ms, 3), "ms");
        gate("propagate_ns", round(propagate_ns, 1), "ns");
        gate("hit_us", round(hit_us, 1), "us");
        gate("parse_ms", round(parse_ms, 2), "ms");
        for ((name, _, _), v) in VISIBLE_CAMERAS.iter().zip(counts.visible) {
            gate(&format!("visible.{name}"), v as f64, "count");
        }
        gate("upload_kb", counts.upload_kb as f64, "KB");

        // How the moving dots move on a 2x display at rate 1, at each
        // visible camera (`AltitudeFrame::dot_motion`): the fastest
        // on-screen dot, device px per scene second (graded to +-5%, so an
        // under-estimate fails as surely as an over-estimate), and how
        // often they then ask the host for a frame (`MapView::paint`: once
        // that dot has moved `DOT_REDRAW_PX`, an off-screen one could
        // enter, or an object appears or vanishes). Every such frame
        // redraws the host's whole gpui window; before this was bounded
        // they asked for every display refresh (120 Hz on ProMotion). And
        // the per-paint cost of the bound: one Hermite and one projection
        // per object.
        for (name, zoom, lat) in VISIBLE_CAMERAS {
            let f = retina(frame(zoom, lat));
            let motion = f.dot_motion(&instances, true, 1.0);
            let wait = dot_redraw_after_s(1.0, motion).expect("a running clock");
            gate(&format!("dots_px_s.{name}"), round(motion.px_per_scene_s, 3), "px/s");
            gate(&format!("dots_redraw_hz.{name}"), round(1.0 / wait, 1), "hz");
        }
        let f = retina(frame(VISIBLE_CAMERAS[2].1, VISIBLE_CAMERAS[2].2));
        let mut scans: Vec<f64> = (0..7)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(f.dot_motion(std::hint::black_box(&instances), true, 1.0));
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        scans.sort_by(f64::total_cmp);
        gate("dots_speed_us", round(scans[scans.len() / 2], 1), "us");
    }

    /// `f` on a 2x display: device-px viewport, as `MapView::paint` sets it.
    fn retina(f: AltitudeFrame) -> AltitudeFrame {
        AltitudeFrame { viewport: (VIEWPORT.0 * 2.0, VIEWPORT.1 * 2.0), px_scale: 2.0, ..f }
    }

    /// `--measure-look`: the `look` section's `gate:` lines. The textures
    /// are seeded noise at the real assets' sizes (3840x1920 stars,
    /// 2048x1024 clouds): an RGBA8 texture's sampling cost depends on its
    /// size and the access pattern, not on what the texels show, and the
    /// gate must never need the NASA downloads.
    pub fn look() {
        use gpui_maps::render::space_look::{measure_frame_cost, EquirectImage, SpaceLook, SpaceLookPipeline, SpaceTextures, VIEWPORT_SUN};
        fn noise(width: u32, height: u32, seed: u64) -> EquirectImage {
            let mut x = seed;
            let rgba = (0..width as usize * height as usize * 4)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    (x >> 56) as u8
                })
                .collect();
            EquirectImage { width, height, rgba }
        }
        let textures = SpaceTextures { starmap: Some(noise(3840, 1920, 0x5eed)), clouds: Some(noise(2048, 1024, 0xc10d)) };
        let ctx = pollster::block_on(gpui_maps::render::GpuContext::new_headless()).expect("headless device");
        let (color, depth) = (wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Depth24PlusStencil8);
        // The first-use build on the frame thread: shader, two pipelines,
        // the ~37 MB upload, to device idle. One untimed build compiles the
        // shader (Metal caches it on disk after), then best of 3.
        drop(SpaceLookPipeline::new(&ctx, color, depth, &textures, 1));
        let (build_ms, look) = {
            let mut best = f64::INFINITY;
            let mut kept = None;
            for _ in 0..3 {
                let t = Instant::now();
                let p = SpaceLookPipeline::new(&ctx, color, depth, &textures, 1);
                let _ = ctx.device.poll(wgpu::PollType::wait_indefinitely());
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
                kept = Some(p);
            }
            (best, kept.expect("three builds"))
        };
        // The per-frame CPU: the uniform and its 128-byte write.
        let camera = Camera { center_lon: 100.0, center_lat: 20.0, zoom: 3.0, pitch: 0.0, bearing: 0.0, viewport_width_px: 3456.0, viewport_height_px: 2234.0, fov_y: Camera::DEFAULT_FOV_Y };
        let frame = gpui_maps::render::globe::globe_frame(&camera);
        let mut writes: Vec<f64> = (0..5)
            .map(|_| {
                let t = Instant::now();
                for i in 0..2_000 {
                    look.write(&ctx, &camera, &frame, &SpaceLook::ON, VIEWPORT_SUN, f64::from(i) * 1e-4);
                }
                t.elapsed().as_secs_f64() * 1e6 / 2_000.0
            })
            .collect();
        writes.sort_by(f64::total_cmp);
        // The GPU: both passes over a clear-only pass, z3 (sky, clouds,
        // shading and rim all drawn), at a 2x 16" MacBook's 3456x2234 and a
        // 5K display's 5120x2880.
        let mut draws = 0;
        let mut gpu = |size| {
            let c = measure_frame_cost(&ctx, &look, color, depth, size, 3.0, 9, 20);
            draws += c.draws;
            ((c.look_ms - c.clear_ms) * 1e3).max(1.0)
        };
        let gpu_2x = gpu((3456, 2234));
        let gpu_5k = gpu((5120, 2880));
        if draws == 0 {
            eprintln!("GATE ERROR: the look issued no draw call; nothing was timed");
            std::process::exit(2);
        }
        println!("look: synthetic 3840x1920 + 2048x1024 textures; z3, lat 20");
        println!("gate: look.build_ms {} ms", round(build_ms, 2));
        println!("gate: look.write_us {} us", round(writes[writes.len() / 2], 2));
        // In microseconds, not ms: the harness gives every ms time row a
        // ceiling of at least baseline + 1 ms, which on a ~0.3 ms figure
        // would let the look quadruple unseen.
        println!("gate: look.gpu_2x_us {} us", round(gpu_2x, 1));
        println!("gate: look.gpu_5k_us {} us", round(gpu_5k, 1));
        println!("gate: look.draw_calls {draws} count");
    }

    fn round(v: f64, places: i32) -> f64 {
        let k = 10f64.powi(places);
        (v * k).round() / k
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::measure::measure_counts;

    /// The section's Exact lines: the same on every run, one upload of
    /// every synthetic object, and a visible count a broken occlusion (all
    /// hidden, or none hidden) could not produce.
    #[test]
    fn the_measure_counts_are_deterministic() {
        let a = measure_counts(); // the three visible counts and upload_kb, no timing
        assert_eq!(a, measure_counts());
        assert_eq!(a.upload_kb, (10_000 * 60usize).div_ceil(1024));
        assert!(a.visible[0] > 1_000 && a.visible[0] < 10_000, "{:?}", a.visible);
    }

    #[test]
    fn at_parses_utc_with_or_without_the_z_and_a_space() {
        let t = super::native::parse_utc("2026-09-21T14:13:20Z").unwrap();
        assert_eq!(t, 1_790_000_000.0);
        assert_eq!(super::native::parse_utc("2026-09-21 14:13:20"), Some(t));
        assert!(super::native::parse_utc("yesterday").is_none());
    }
}

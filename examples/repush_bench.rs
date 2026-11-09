//! Re-push rate vs tiling: a ~53k-point, ~500-route line source re-pushed
//! through `set_source` at 1, 2.5 and 5 pushes per second while frames are
//! drawn at 30 fps, on the whole-source path and the tiled path,
//! INTERLEAVED (untiled, tiled, untiled, tiled, ...) so machine load lands
//! on both. See `gpui_maps::overlay::repush_bench` for what one run does.
//!
//!   cargo run --release --features probe --example repush_bench
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `REPUSH_RATES` | `1,2.5,5` | pushes per second |
//! | `REPUSH_SECONDS` | 20 | seconds per run |
//! | `REPUSH_REPS` | 2 | interleaved repetitions |
//! | `REPUSH_ROUTES` / `REPUSH_POINTS` | 500 / 106 | routes, points per route |
//! | `REPUSH_CHANGE` | 0.05 | fraction of routes moved per push |
//! | `REPUSH_PAINT` | unset | `declared`: data-driven `line-color` expression |

use gpui_maps::overlay::repush_bench::{percentile, run, Config, FrameSample, Report};

fn env<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn load_1m() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().nth(1).map(str::to_owned))
        .unwrap_or_else(|| "?".into())
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn summarise(label: &str, config: &Config, report: &Report, load_before: &str) {
    let frames = &report.frames;
    let col = |f: fn(&FrameSample) -> u64| frames.iter().map(f).collect::<Vec<u64>>();
    let cpu = col(|s| s.cpu);
    let frame = col(|s| s.cpu + s.gpu_wait);
    let pushes: Vec<u64> = frames.iter().filter(|s| s.push > 0).map(|s| s.push).collect();
    let installs = col(|s| s.install);
    let uploads = col(|s| s.upload);
    let draws = col(|s| s.draw);
    let gpu = col(|s| s.gpu_wait);
    // Attribution: in the frames over 33 ms of CPU, which phase is largest.
    let slow: Vec<&FrameSample> = frames.iter().filter(|s| s.cpu > 33_000_000).collect();
    let share = |f: fn(&FrameSample) -> u64| slow.iter().map(|s| f(s)).sum::<u64>() as f64 / 1e6;
    let pcts = |v: &[u64]| format!("{:.1}/{:.1}/{:.1}/{:.1}", ms(percentile(v, 50.0)), ms(percentile(v, 95.0)), ms(percentile(v, 99.0)), ms(*v.iter().max().unwrap_or(&0)));
    println!(
        "row: {label} rate={} tiled={} frames={} pushes={} landed={} tile_bakes={} blank_after_paint={} verts/bake={} load1m_before={load_before} load1m_after={}",
        config.rate_hz,
        config.tiled,
        frames.len(),
        report.pushes,
        report.landed,
        report.tile_bakes,
        report.blank_frames_after_first_paint,
        report.vertices_per_bake,
        load_1m(),
    );
    println!("  cpu   p50/p95/p99/max ms  {}", pcts(&cpu));
    println!("  frame p50/p95/p99/max ms  {}  (cpu + gpu wait)", pcts(&frame));
    println!("  push  p50/p95/p99/max ms  {}", pcts(&pushes));
    println!("  install p50/p95/p99/max   {}", pcts(&installs));
    println!("  upload  p50/p95/p99/max   {}", pcts(&uploads));
    println!("  draw    p50/p95/p99/max   {}", pcts(&draws));
    println!("  gpu     p50/p95/p99/max   {}", pcts(&gpu));
    let upload_bytes: u64 = frames.iter().map(|s| s.upload_bytes).max().unwrap_or(0);
    println!(
        "  slow frames (cpu>33ms): {}  ms in push {:.1} install {:.1} upload {:.1} draw {:.1}  | max upload {:.1} MB | worker {:.0} ms total, {:.1} ms/push",
        slow.len(),
        share(|s| s.push),
        share(|s| s.install),
        share(|s| s.upload),
        share(|s| s.draw),
        upload_bytes as f64 / 1e6,
        ms(report.worker_ns),
        ms(report.worker_ns) / report.pushes.max(1) as f64,
    );
    // Main-thread cost per push: every frame-thread ms above the no-push
    // floor (the median frame), summed, per push.
    let floor = percentile(&cpu, 50.0);
    let extra: u64 = cpu.iter().map(|c| c.saturating_sub(floor)).sum();
    println!("  main-thread ms per push (above the p50 floor): {:.1}", ms(extra) / report.pushes.max(1) as f64);
    let n = report.attributed.max(1) as f64;
    println!(
        "  set_source split (off-clock re-run, mean ms): parse {:.1} plan {:.1} hit_index {:.1}",
        ms(report.parse_ns) / n,
        ms(report.plan_ns) / n,
        ms(report.hit_ns) / n
    );
}

fn main() -> anyhow::Result<()> {
    let rates: Vec<f64> = std::env::var("REPUSH_RATES")
        .unwrap_or_else(|_| "1,2.5,5".into())
        .split(',')
        .filter_map(|r| r.trim().parse().ok())
        .collect();
    let reps: usize = env("REPUSH_REPS", 2);
    let base = Config {
        seconds: env("REPUSH_SECONDS", 20.0),
        routes: env("REPUSH_ROUTES", 500),
        points_per_route: env("REPUSH_POINTS", 106),
        change_fraction: env("REPUSH_CHANGE", 0.05),
        declared_paint: std::env::var("REPUSH_PAINT").as_deref() == Ok("declared"),
        ..Config::default()
    };
    println!(
        "repush_bench: {} routes x {} points = {} points, {} fps, {} s/run, reps {reps}",
        base.routes,
        base.points_per_route,
        base.routes * base.points_per_route,
        base.fps,
        base.seconds
    );
    for rep in 0..reps {
        for &rate in &rates {
            for tiled in [false, true] {
                let config = Config { rate_hz: rate, tiled, seed: 1, ..base.clone() };
                let load = load_1m();
                let report = run(&config)?;
                summarise(&format!("rep{rep}"), &config, &report, &load);
            }
        }
    }
    Ok(())
}

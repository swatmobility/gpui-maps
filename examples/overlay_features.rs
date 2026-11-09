//! Singapore as a GeoJSON showcase -- planning areas by population
//! density, the MRT/LRT network in its line colours, stations sized by the
//! lines they serve -- in its own window. The map itself is the panel the
//! gallery also shows (`overlay-features`), in
//! `kit/src/panels/overlay_features.rs`; this file is its standalone
//! window.
//!
//! The data is downloaded on first open into
//! `~/.cache/gpui-map-data/singapore/` (or fetch it ahead with
//! `scripts/fetch-singapore-overlay.sh`); nothing is committed.
//!
//! Run with: cargo run --release --example overlay_features
//!
//! `--capture DIR` opens NO window: it renders the overlay headless over
//! Positron (`gpui_maps::capture`, flat, labels on) at two cameras -- all
//! of Singapore at z10.8 and the city centre at z13 -- and writes
//! `DIR/overlay-singapore-z10.8.png` and `DIR/overlay-city-z13.png`. The
//! capture draws one overlay source, so it pushes the panel's areas,
//! tracks and stations as one collection with the panel's one paint
//! declaration; station names (their own source in the panel) are not in it.

use std::path::{Path, PathBuf};

use gpui::{px, size, App, Bounds, TitlebarOptions, WindowBounds, WindowOptions};

use swat_maps_kit::kit;

use swat_maps_kit::panels::overlay_features as panel;

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--capture") {
        let dir = PathBuf::from(args.get(i + 1).expect("--capture DIR"));
        // `--camera NAME,LON,LAT,ZOOM` (repeatable) replaces the two defaults.
        let cameras: Vec<(String, (f64, f64, f64))> = args
            .windows(2)
            .filter(|w| w[0] == "--camera")
            .filter_map(|w| {
                let p: Vec<&str> = w[1].split(',').collect();
                Some((p.first()?.to_string(), (p.get(1)?.parse().ok()?, p.get(2)?.parse().ok()?, p.get(3)?.parse().ok()?)))
            })
            .collect();
        // `--highlight-area NAME` draws that area's hover highlight too.
        let highlight = args.windows(2).find(|w| w[0] == "--highlight-area").map(|w| w[1].clone());
        if let Err(err) = capture(&runtime, &dir, cameras, highlight) {
            eprintln!("overlay_features --capture: {err:#}");
            std::process::exit(1);
        }
        return;
    }
    let handle = runtime.handle().clone();

    let title = "Overlay Features".to_string();
    let env = kit::PanelEnv::from_process(kit::Host::Standalone { title: title.clone() }, handle, (1024.0, 768.0));

    gpui_platform::application().run(move |cx: &mut App| {
        let env = env.clone();
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.clone().into()), ..Default::default() }),
                ..Default::default()
            },
            move |window, cx| {
                let built = panel::build(&env, window, cx).unwrap_or_else(|err| {
                    eprintln!("overlay_features: {err:#}");
                    std::process::exit(1);
                });
                built
                    .view
                    .downcast::<panel::OverlayPanel>()
                    .unwrap_or_else(|_| panic!("overlay_features's panel root is an OverlayPanel"))
            },
        )
        .expect("failed to open window");
    });

    std::mem::forget(runtime);
}

/// The two review frames, headless. See the module doc.
fn capture(
    runtime: &tokio::runtime::Runtime,
    out: &Path,
    cameras: Vec<(String, (f64, f64, f64))>,
    highlight: Option<String>,
) -> anyhow::Result<()> {
    use gpui_maps::capture::{capture, fetch_cover, CaptureRequest};
    let data = panel::data_dir();
    if !panel::is_cached(&data) {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        runtime
            .block_on(panel::download_missing(data.clone(), cancel, |done, total, file| eprintln!("download {done}/{total} {file}")))
            .map_err(anyhow::Error::msg)?;
    }
    let started = std::time::Instant::now();
    let prepared = panel::prepare(&data).map_err(anyhow::Error::msg)?;
    eprintln!(
        "prepared in {:.2} s: {} areas ({} -> {} vertices), {} tracks ({} -> {} vertices), {} stations, unjoined {:?}",
        started.elapsed().as_secs_f64(),
        prepared.areas.len(),
        prepared.area_vertices.0,
        prepared.area_vertices.1,
        prepared.tracks.len(),
        prepared.track_vertices.0,
        prepared.track_vertices.1,
        prepared.stations.len(),
        prepared.unjoined
    );
    let overlay = panel::combined_geojson(&prepared);
    // Areas come first in the combined collection, so an area's index is
    // its feature index there.
    let highlight = highlight
        .map(|name| {
            prepared
                .areas
                .iter()
                .position(|a| panel::normalise(&a.area.name) == panel::normalise(&name))
                .map(|i| (i, panel::area_highlight_style()))
                .ok_or_else(|| anyhow::anyhow!("--highlight-area {name}: no such area"))
        })
        .transpose()?;
    let style = kit::compile_style("demos/positron-style.json")?;
    let source = kit::openfreemap_planet();
    let cache = std::env::temp_dir().join("gpui-map-capture-tiles-openfreemap");
    std::fs::create_dir_all(out)?;
    let cameras = if cameras.is_empty() {
        vec![("overlay-singapore-z10.8".to_string(), panel::START), ("overlay-city-z13".to_string(), (103.8520, 1.2900, 13.0))]
    } else {
        cameras
    };
    for (name, (lon, lat, zoom)) in cameras {
        let request = CaptureRequest {
            center_lon: lon,
            center_lat: lat,
            zoom,
            width: 1280,
            height: 800,
            flat: true,
            flat_tile_zoom_max: 14,
            labels: true,
            overlay: Some(overlay.clone()),
            overlay_paint: Some(panel::paint_json().to_string()),
            overlay_highlight: highlight,
            ..CaptureRequest::default()
        };
        let tiles = runtime.block_on(fetch_cover(source.as_ref(), &request.cover(), &cache))?;
        let frame = capture(&request, &tiles, &style)?;
        let path = out.join(format!("{name}.png"));
        frame.save_png(&path)?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

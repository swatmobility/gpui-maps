<!--
Moves with docs/public/README.md at the cut; links are relative to the repository root.
-->

# Getting started with SWAT Maps for GPUI

This guide takes you from a fresh Mac to a map in your own GPUI
application: run the example, add the crate, change the style, put your own
data on the map, and go offline. Every API named here is in `src/`; the
file it lives in is given so you can read its documentation.

## 1. Install Rust

The desktop crate builds on **macOS** (it draws through Metal). Install the
stable toolchain with [rustup](https://rustup.rs):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustc --version     # the current release is built with 1.98.1
```

You also need the Xcode command-line tools (`xcode-select --install`).

## 2. Run the example

```bash
git clone https://github.com/swatmobility/gpui-maps
cd gpui-maps
cargo run --example hello_map
```

A window opens on Singapore at zoom 12, drawn from OpenFreeMap's planet
tiles with the Liberty style. No account and no token are needed. The
first build compiles GPUI and `wgpu` and takes several minutes; later
builds are incremental.

Drag to pan, scroll to zoom toward the cursor, double-click to zoom in,
and Shift+drag to rotate or tilt. At low zoom the map is a globe.

## 3. Add a map to your own GPUI app

### Cargo.toml

```toml
[dependencies]
gpui-maps = "0.8"
# GPUI from crates.io, renamed so `gpui::` paths work; pin the exact version
# gpui-maps is built against.
gpui = { package = "gpui-pre", version = "=0.3.12" }
# The platform layer. Keep `font-kit` on: without it macOS renders no text.
gpui-platform = { package = "gpui-pre-platform", version = "=0.3.12", features = ["font-kit"] }
# MapView fetches and decodes tiles on a Tokio runtime you hand it.
tokio = { version = "1", features = ["rt-multi-thread"] }
anyhow = "1"

# Tessellation in an unoptimised debug build is too slow to pan smoothly;
# optimise the map crate even in dev builds.
[profile.dev.package.gpui-maps]
opt-level = 2
```

### main.rs

This is `examples/hello_map.rs`:

```rust
use gpui::{prelude::*, px, size, App, Bounds, WindowBounds, WindowOptions};
use gpui_maps::basemaps::{self, Basemap};
use gpui_maps::map_view::MapView;

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let rt = runtime.handle().clone();
    gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1024.0), px(768.0)), cx);
        let options = WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() };
        cx.open_window(options, move |_window, cx| {
            let b = basemaps::load(Basemap::Liberty).expect("the bundled Liberty style compiles");
            // Singapore, zoom 12; the view resizes itself to the window.
            let map = cx.new(|_| MapView::new(103.85, 1.29, 12.0, 1024.0, 768.0, b.style, b.source, rt.clone()));
            if let Some(sprite) = b.sprite {
                basemaps::spawn_sprites(map.downgrade(), sprite, None, rt, cx);
            }
            map
        })
        .expect("open the window");
    });
    std::mem::forget(runtime); // the app owns the process from here
}
```

What each piece does:

- `MapView::new(lon, lat, zoom, width_px, height_px, style, source, runtime)`
  (`src/map_view.rs`) creates the map element. The size is the initial
  viewport; the view follows its element's bounds after that.
- `basemaps::load(Basemap::...)` (`src/basemaps/mod.rs`) returns a
  `LoadedBasemap`: the compiled `style`, an OpenFreeMap tile `source` that
  already carries its credit, the `sprite` base URL, and the raw style
  `document`.
- `basemaps::spawn_sprites` fetches the icon sheet in the background (the
  `sprite_http_fetch` feature, on by default). Pass `None` for the token:
  the open basemaps need none.

`MapView` never takes keyboard focus on its own; that is the host's
decision. For the keyboard rotate and tilt keys, focus it once its focus
handle exists (it is `None` until the first paint):

```rust
if let Some(handle) = map.read(cx).focus_handle() {
    window.focus(&handle, cx);
}
```

## 4. Change the style

### Another bundled basemap

`Basemap::ALL` lists the six embedded styles: `StreetsLight`, `Liberty`,
`Positron`, `Dark`, `OpenDark` and `SwatDark`. All draw OpenFreeMap's
planet tiles, so switching only needs the new style:

```rust
let dark = basemaps::load(Basemap::Dark)?;
map.update(cx, |view, cx| {
    view.set_style(dark.style);
    cx.notify();
});
```

`Basemap::name()` and `Basemap::is_dark()` help build a picker.

### Your own style JSON

Any MapLibre-compatible style document (style spec v8) compiles with
`style_spec::parse_and_compile` (`src/style_spec/mod.rs`). Give it a tile
source that serves the schema the style was written for, with the data
provider's credit:

```rust
use std::sync::Arc;
use gpui_maps::style_spec;
use gpui_maps::tile_source::{http::HttpTileSource, TileSource};

// OpenFreeMap styles: https://tiles.openfreemap.org/styles/liberty,
// .../positron, .../dark (OpenMapTiles schema). Liberty is vendored as
// examples/liberty-style.json.
let style = style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json")?)?;
let source: Arc<dyn TileSource> = Arc::new(HttpTileSource::new(
    "https://tiles.example.org/{z}/{x}/{y}.pbf", // your OpenMapTiles-schema tiles
    14,                                          // the source's max zoom
    "© OpenStreetMap contributors",              // drawn on the map, bottom-right
));
let map = cx.new(|_| MapView::new(103.85, 1.29, 12.0, 1024.0, 768.0, style, source, rt.clone()));
```

The engine draws the source's credit on every map and has no API to hide
it. Give every source the credit its data licence requires.

Mapbox styles and tiles also work, but they are off by default: they need
`GPUI_MAP_ALLOW_MAPBOX=1` and your own token in `MAPBOX_ACCESS_TOKEN`.

## 5. Add your own data

Overlay data is GeoJSON, pushed into a named source. Paint is read from
each feature's `properties`, using the style spec's own property names
(`fill-color`, `fill-opacity`, `line-color`, `line-width`,
`line-dasharray`, `circle-color`, `circle-radius`, `icon-image`,
`text-field` and others; see `src/overlay/paint.rs`):

```rust
let stops = r##"{
  "type": "FeatureCollection",
  "features": [
    { "type": "Feature",
      "geometry": { "type": "Point", "coordinates": [103.8519, 1.2966] },
      "properties": { "circle-radius": 8, "circle-color": "#e4572e", "text-field": "City Hall" } },
    { "type": "Feature",
      "geometry": { "type": "LineString", "coordinates": [[103.8454, 1.2789], [103.8519, 1.2966], [103.8606, 1.3006]] },
      "properties": { "line-color": "#1d4ed8",
                      "line-width": ["interpolate", ["linear"], ["zoom"], 10, 2, 16, 6] } }
  ]
}"##;

map.update(cx, |view, cx| {
    if let Err(error) = view.set_source("stops", stops, cx) {
        eprintln!("rejected: {error}");
    }
    view.set_source_credit("stops", Some("Stops: my own data"));
});
```

- `MapView::set_source(name, geojson, cx)` replaces everything in that
  source. An empty collection clears it, and a rejected call changes
  nothing. `MapView::set_features(geojson, cx)` does the same for the
  default source.
- `line-width` accepts a zoom expression; the other properties are
  constants per feature.
- `MapView::remove_source(name, cx)` removes a source.
- `MapView::set_source_credit(name, credit)` adds the source's credit to
  the on-map attribution.

`examples/simulation_overlay.rs` builds a real overlay (route casings,
SDF stop pins, labels) this way. For large inputs, the overlay readers in
`src/overlay/` take CSV (`overlay::csv`), FlatGeobuf (`overlay::fgb`) and
GeoParquet (`overlay::geoparquet`) directly; the `demos/csv_points.rs`,
`demos/fgb.rs` and `demos/geoparquet.rs` demos push a million rows through
each.

### Drawing your own UI over the map

On macOS the map draws into its own Metal layer, which composites above
GPUI's content by default, so a GPUI element placed over the map is hidden
under it. Call `MapView::set_chrome_above_map(true)` once, before the first
paint, to put the map below GPUI's layer instead. That makes the window
background transparent: keep the elements between the window root and the
map without an opaque background, or they cover the map. An element over
the map that should take the mouse must call `.occlude()`, or the map
underneath also receives the drag.

### Globe or flat

The globe is on by default and blends into Mercator as you zoom in.
`MapView::set_globe_enabled(false)` keeps the map flat at every zoom.

## 6. Go offline

Point a `PmtilesSource` at a local `.pmtiles` archive and the map renders
with no network at all:

```rust
use gpui_maps::tile_source::pmtiles::PmtilesSource;

let source = rt.block_on(PmtilesSource::open("singapore.pmtiles"))?; // mmaps the file
let style = style_spec::parse_and_compile(&std::fs::read("examples/liberty-style.json")?)?;
let map = cx.new(|_| MapView::new(103.85, 1.29, 12.0, 1024.0, 768.0, style, Arc::new(source), rt.clone()));
```

The archive's own TileJSON `attribution` becomes the on-map credit. Labels
are shaped from the system's fonts, so no glyph server is involved. Icons
come from `gpui_maps::cached_sprite_atlas`, which fetches a sprite sheet
once into `~/.cache/gpui-map/sprites/` and reads it from there afterwards;
with no cache and no network the map draws without icons.

Run the worked example with `PMTILES_PATH=singapore.pmtiles cargo run
--release --example offline_map`. [`docs/OFFLINE.md`](docs/OFFLINE.md)
explains how to build an archive for a region, what it costs on disk, how
to verify a run with the network denied, and what offline does not
include (routing and place search).

## 7. iOS and the browser

The same engine source builds for two more shells, each its own Cargo
workspace:

- **Browser** (`web/`): wasm on WebGPU, with WebGL2 as the fallback (`?gl`
  in the page URL forces it). `./web/run.sh` builds and serves it; it needs
  the `wasm32-unknown-unknown` target and the `wasm-bindgen` CLI at the
  version `web/Cargo.toml` pins.
- **iOS** (`ios/`): UIKit and Metal. `./ios/run.sh` builds it, installs it
  on a booted simulator and launches it.
- **Embedding in a native iOS app** (`embed-ios/`): a Rust library the
  app's own Rust core depends on, with Swift bindings generated by uniffi.
  See [`embed-ios/swift/README.md`](embed-ios/swift/README.md).

`./scripts/check-wasm.sh` and `./scripts/check-ios.sh` check both builds
from the repository root.

Turn-by-turn navigation runs on all three: see
[`docs/navigation.md`](docs/navigation.md) for the simulated drive and how
a `MapView` host turns navigation input on (`MapView::set_nav_enabled`; off
by default).

## Troubleshooting

**The map draws but there is no text.** The platform crate was built
without its `font-kit` feature. Use the `gpui-platform` line from step 3.

**Rotate and tilt keys do nothing.** The map does not have keyboard focus.
`MapView` never focuses itself; see the focus snippet at the end of
step 3.

**A panel I draw over the map is invisible.** See "Drawing your own UI
over the map" in step 5.

**A closed map pane keeps showing its last frame.** The map's Metal layer
lives outside GPUI's element tree, so a `MapView` you stop rendering keeps
compositing. Call `MapView::set_hidden(true)` to take it off screen
without discarding what it has loaded.

**Measurements read zero frames.** A window that is not frontmost is not
painted. The performance tools report this as "occluded paints" and void
the run; see `docs/perf-gate.md`.

**A Mapbox example shows a dark, empty map.** With Mapbox enabled, a wrong
or expired token makes every tile request fail with HTTP 401, and only the
style's background colour is drawn. Check stderr for 401, or leave Mapbox
off: every example has an open look-alike.

**Debug builds pan slowly.** Add the `[profile.dev.package.gpui-maps]`
section from step 3, or run with `--release`.

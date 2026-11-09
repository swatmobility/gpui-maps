# gpui-maps in a native iOS app

The engine ships as a Rust library crate that the app's own Rust core
depends on; the app keeps ONE staticlib (two Rust staticlibs in one app
collide on std's symbols).

## Rust side (the app's core)

```toml
# Cargo.toml of the app core (uniffi must be =0.31.2, the version this crate uses)
gpui-maps-embed = { git = "https://github.com/<owner>/gpui-map", rev = "<rev>" }

# A debug-built tessellator is ~10x too slow to pan: optimise the map even in dev.
[profile.dev.package.gpui-maps-embed]
opt-level = 2
[profile.dev.package.swat-maps-ios]
opt-level = 2
```

```rust
// lib.rs of the app core: keep your own `uniffi::setup_scaffolding!()`, and
// make sure the map crate is linked (an unused dependency is dropped).
pub use gpui_maps_embed;
```

`gpui-maps-embed` calls `uniffi::setup_scaffolding!()` itself, as uniffi
requires of every crate that exports. Library-mode bindgen over the app's
`.a` then writes, beside the app's own files, `gpui_maps_embed.swift`,
`gpui_maps_embedFFI.h` and `gpui_maps_embedFFI.modulemap`: add all three to
the app the way the core's own three are added.

Build with `IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --target aarch64-apple-ios`.

### Frameworks

The map links Metal, QuartzCore, CoreGraphics and Foundation (and calls
UIKit through the Objective-C runtime, which every SwiftUI app links). No
AVFoundation/AVFAudio: turn-by-turn speech is behind the `swat-maps-ios`
`speech` feature, which this crate leaves off. Tiles come over `ureq` with
rustls and bundled webpki roots, so no Security/SystemConfiguration calls of
its own; an app that links those for its own networking is unaffected.

### 120 Hz

ProMotion phones present at 60 Hz unless the app's Info.plist sets
`CADisableMinimumFrameDurationOnPhone` to `YES`; the glue's display link
then asks for the screen's maximum.

### Layer styles

`setStyle(layer:paintJson:)` takes a style-spec paint object:

- `circle-*`, `line-*`, `fill-*` constants, and `["get", k]` for the colours
  and `circle-radius`;
- on point layers, labels: `text-field` (`["get", "label"]` or
  `"{code}\n{name}"`), `text-size` / `text-color` (constant or `["get", k]`),
  and constant `text-offset`, `text-anchor`, `text-halo-color`,
  `text-halo-width`, `text-max-width`. Labels collide with each other and
  with the basemap's; an empty label draws no text.
- Colours: `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()`, `rgba()`.
- `min-zoom` / `max-zoom` (or `minzoom` / `maxzoom`) on any layer: drawn,
  labelled and tappable while `min <= zoom < max`, as in MapLibre.
- Style and features may come in either order, with frames between them.

Overlays draw above the basemap's 3D buildings, so a route under a tower
stays whole. `setBuildings3d(on: false)` lays the buildings flat (their
footprints, same colour) without reloading the style.

MapLibre's layer model: a point layer whose style sets only `text-*` (or
`icon-*`) keys is a symbol layer and draws no circle; it draws one when a
`circle-*` key is set, or when the layer has no style at all (the default
dot).

Labels use the bundled Latin font for now: Japanese, Chinese, Thai and other
scripts need the iOS system-font loader, which is the next engine task.

### Right to left

The glue passes `effectiveUserInterfaceLayoutDirection` to
`setLayoutDirection(rtl:)`: right to left moves the compass to the top-left.
The SWAT mark stays bottom-left and the credits bottom-right (their text
shapes right to left by itself), as MapLibre keeps its attribution.

### Privacy

Diagnostic lines are off by default (`setDebugLogging(on:)`), and even when
on they never carry a coordinate, a tile x/y, a touch point or a URL.

## Swift side

Copy `GpuiMapView.swift` into the app target. It is the `CAMetalLayer`-backed
`UIView`, the `CADisplayLink` that runs only while the engine owes a frame,
the touch forwarding, and a `UIViewRepresentable`:

```swift
MapViewRepresentable(
    onReady: { engine in
        engine.jumpTo(update: CameraUpdate(lon: 103.8562, lat: 1.2885, zoom: 14.6))
        try? engine.setStyle(layer: "route", paintJson: ##"{"line-width": 5, "line-color": "#0a84ff"}"##)
        try? engine.upsertLines(layer: "route", features: [LineFeature(id: "R10", coords: shape, props: [:])])
        try? engine.upsertPoints(layer: "stops", features: stops.map {
            PointFeature(id: $0.id, lon: $0.lon, lat: $0.lat, props: [:]) })
    },
    onTap: { layer, id in selected = (layer, id) },
    onCameraChanged: { camera in zoom = camera.zoom },   // main queue
    extraInsets: UIEdgeInsets(top: 0, left: 0, bottom: sheetHeight, right: 0)
)
.ignoresSafeArea()
```

`ExampleApp.swift` is a whole sample (stops, a route, buses moving by id).

Threading: every `MapEngine` call on the main thread. `onNeedsFrame` arrives on
any thread and must only unpause the display link (the glue does this).

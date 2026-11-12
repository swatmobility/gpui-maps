<!--
This file becomes the repository's root CHANGELOG.md at the cut
(scripts/split-lib-transform.py moves it).
-->

# Changelog

All notable changes to SWAT Maps for GPUI are recorded here. The crates
follow [Semantic Versioning](https://semver.org); while they are on `0.x`, a
minor version may break the API (see `docs/versioning.md`).

## 0.8.0 — the first public release

The first open-source release of the engine and its format crates, under
MIT OR Apache-2.0. Built on gpui-pre `=0.3.12`, the gpui that gpui-kit 0.9.1
links. Versions of `gpui-maps` before 0.8.0 were internal and never
published; the format crates and `gpui-agentic-http` start at 0.1.0.

- `gpui-maps`: the GPU vector-map engine for GPUI. MVT over HTTP,
  PMTiles, MBTiles and raster tiles; MapLibre / Mapbox GL style JSON (style
  spec v8) with expressions; six keyless basemaps over OpenFreeMap; a globe
  that blends into flat Mercator, with atmosphere and sky; `fill-extrusion`
  buildings, overlay extrusion and glTF models; GeoJSON, CSV, FlatGeobuf,
  GeoParquet and Cloud-Optimised GeoTIFF overlays with picking; arcs,
  animated trips, heatmaps, hexagon and grid binning and dot density;
  turn-by-turn route following; map stories rendered to video.
- `swat-maps-doc`: the map document format.
- `swat-maps-dash`: the dashboard format and its SQL compiler.
- `swat-maps-stac`: STAC catalogue reading.
- `gpui-agentic-http`: a local-only, self-describing HTTP surface for
  driving and inspecting a gpui app.
- The web (wasm) and iOS shells, the Swift embedding, and the examples.

# swat-maps-stac

The crate is published as `swat-maps-stac`; its library path is `swat_maps_stac`.

A lenient reader for static [STAC](https://stacspec.org) 1.1 catalogs and their
Portolan profile, part of [SWAT Maps](https://github.com/swatmobility/gpui-maps):
catalogs, collections, items, assets by role, links, licences and providers,
MapLibre styles for the PMTiles derivative, and an on-disk HTTP cache that
revalidates with ETag / Last-Modified. Spec deviations become warnings, never
refusals.

Pure Rust with no HTTP client of its own: the host supplies a `Transport`, so
the crate builds for `wasm32-unknown-unknown` and iOS unchanged.

Modules: `model`, `catalog`, `cache`, `style`, `licence`, `portolan` and
`agents_md`.

See the repository README for the licence and attribution.

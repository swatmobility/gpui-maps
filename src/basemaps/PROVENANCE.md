# Keyless basemap styles: provenance and licences

`gpui_maps::basemaps` embeds six style documents with `include_bytes!`. They
are embedded from the paths the repository already vendored them at (not
copied here), so the examples, the iOS/web shells, the story fixtures and the
library share one file per style and cannot drift. `embedded_bytes_are_the_vendored_files`
pins the bytes to those paths.

Checked 2026-10-10 against
[openfreemap-styles `LICENSE.md`](https://github.com/hyperknot/openfreemap-styles/blob/main/LICENSE.md)
and the upstream style repositories.

| `Basemap` | Embedded file | Origin | Style licence |
|---|---|---|---|
| `Liberty` | `examples/liberty-style.json` | OpenFreeMap Liberty (`tiles.openfreemap.org/styles/liberty`), a fork of [maputnik/osm-liberty](https://github.com/maputnik/osm-liberty) (itself OSM Bright, derived from Mapbox Open Styles). One local change: `building-3d` filters `hide_3d` (see the file's `metadata`). | code BSD-3-Clause, design CC BY 4.0 |
| `Positron` | `demos/positron-style.json` | OpenFreeMap Positron, a fork of [openmaptiles/positron-gl-style](https://github.com/openmaptiles/positron-gl-style), derived from CartoDB Basemaps by Stamen and Paul Norman (CC BY 3.0) | code BSD-3-Clause, design CC BY 4.0 |
| `Dark` | `demos/openfreemap-dark-style.json` | OpenFreeMap Dark, [openmaptiles/dark-matter-gl-style](https://github.com/openmaptiles/dark-matter-gl-style) unmodified but for Noto Sans fonts; derived from CartoDB Basemaps (CC BY 3.0). Copyright MapTiler.com & OpenMapTiles contributors, CartoDB Inc. | code BSD-3-Clause, design CC BY 4.0 |
| `StreetsLight` | `examples/styles-open/open-streets-style.json` | Ours: Liberty recoloured by `scripts/gen-open-styles.py` | Liberty's terms for the base (BSD-3-Clause code, CC BY 4.0 design); our modifications are this repository's |
| `OpenDark` | `examples/styles-open/open-dark-style.json` | Ours: Liberty recoloured | as above |
| `SwatDark` | `examples/styles-open/open-swat-routes-dark-style.json` | Ours: Liberty recoloured | as above |

The look-alikes copy no Mapbox style document, layer list, sprite or code
(`examples/styles-open/README.md`); no embedded document names a Mapbox host
or carries a token (`no_mapbox_url_or_token_in_any_embedded_style`).

## Everything the styles reference

- **Tiles:** OpenFreeMap planet, OpenMapTiles schema, OpenStreetMap data
  (ODbL 1.0). OpenMapTiles schema code BSD-3-Clause, design CC BY 4.0. The
  required credit is OpenFreeMap's TileJSON `attribution`
  (`OPENFREEMAP_ATTRIBUTION_HTML`), drawn on the map by the engine.
- **Relief raster** (`ne2_shaded`, z0-6): Natural Earth, public domain.
- **Sprite:** OpenFreeMap's `ofm_f384` sheet, fetched at run time, not
  embedded. Maki POI icons (CC0 1.0) plus a public-domain arrow, per
  openfreemap-styles' licence file; OpenFreeMap's own repository is MIT.
- **Glyphs:** the styles name OpenFreeMap's Noto Sans (SIL OFL 1.1) glyph
  server, but this engine shapes labels from system fonts
  (`src/font_fallback.rs`) and never fetches it.

CC BY 4.0 on a style's design means a product shipping it should credit the
style's authors somewhere reachable (an About/credits screen), in addition to
the on-map data credit.

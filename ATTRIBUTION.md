# Attribution

This file lists the data, fonts, icons and images in this repository that
come from someone else, with the licence each one is under and the credit it
asks for. It also lists what the examples and scripts download at run time,
which is not in the repository but which you redistribute if you ship it.

Third-party source code that parts of the engine were ported from is in
[`NOTICE`](NOTICE). The SWAT name and logo are covered by
[`TRADEMARK.md`](TRADEMARK.md).


## The credit the engine draws for you

`MapView` draws the tile source's credit in the bottom-right corner of every
map (`TileSource::attribution`, `MapView::frame_attribution`). That credit
has no public API to hide or recolour it. For the default keyless basemaps
it is OpenFreeMap's TileJSON attribution
(`gpui_maps::basemaps::OPENFREEMAP_ATTRIBUTION_HTML`): OpenFreeMap,
© OpenMapTiles, data from OpenStreetMap. If you swap in another tile source,
give it that source's credit.

The style DESIGNS (below) are CC BY 4.0. The on-map data credit does not
cover them. A product that ships one of the bundled styles should credit the
style's authors somewhere a user can reach, such as an About or credits
screen.

---

## Bundled in the repository

### SWAT logo

| Files | What it is | Terms |
|---|---|---|
| `src/assets/swat-on-light@2x.png`, `src/assets/swat-on-dark@2x.png` | The SWAT Mobility brand mark the engine draws bottom-left on every map (`src/brand_mark.rs`). | A trademark of SWAT Mobility. **Not covered by the code licence.** See [`TRADEMARK.md`](TRADEMARK.md) and `src/assets/PROVENANCE.md`. |

### Map data (OpenStreetMap, ODbL 1.0)

© OpenStreetMap contributors. Available under the Open Database License
1.0, <https://www.openstreetmap.org/copyright>.

| Files | What it is | Recorded in |
|---|---|---|
| `benches/fixtures/singapore_z14.pbf`, `shinjuku_z14.pbf`, `manhattan_z14.pbf` | Three OpenFreeMap vector tiles (OpenMapTiles schema), served by OpenFreeMap | `benches/fixtures/README.md` |
| `examples/stories/data/marina-reclaimed.geojson` | Illustrative outlines derived from OpenStreetMap data | the file's own `license` field |
| `tests/fixtures/routes/*.json` | OSRM-shaped route responses (key stripped) computed over an OpenStreetMap road graph | `kit/src/kit.rs` (the routes credit) and `docs/navigation.md`. No `SOURCE.md` beside the files; see "Unverified" below |

### Basemap style documents

| Files | Origin | Licence |
|---|---|---|
| `examples/liberty-style.json` | OpenFreeMap Liberty, a fork of maputnik/osm-liberty (itself OSM Bright, derived from Mapbox Open Styles). One local change, recorded in the file's `metadata`. | code BSD-3-Clause, design CC BY 4.0 |
| `demos/positron-style.json` | OpenFreeMap Positron, a fork of openmaptiles/positron-gl-style, derived from CartoDB Basemaps by Stamen and Paul Norman (CC BY 3.0) | code BSD-3-Clause, design CC BY 4.0 |
| `demos/openfreemap-dark-style.json` | OpenFreeMap Dark: openmaptiles/dark-matter-gl-style, unmodified but for Noto Sans fonts; derived from CartoDB Basemaps (CC BY 3.0). Copyright MapTiler.com & OpenMapTiles contributors, CartoDB Inc. | code BSD-3-Clause, design CC BY 4.0 |
| `examples/styles-open/open-streets-style.json`, `open-dark-style.json`, `open-swat-routes-dark-style.json` | Liberty, recoloured by `scripts/gen-open-styles.py` | Liberty's terms for the base; the modifications are this repository's |
| `examples/protomaps-basemap-style.json` | Hand-authored in this repository against the Protomaps v3 layer vocabulary (the file's `metadata`) | this repository's |

Sources: `src/basemaps/PROVENANCE.md` (checked 2026-10-10 against
openfreemap-styles `LICENSE.md`) and `examples/styles-open/README.md`. The
style code's BSD-3-Clause notice belongs in `NOTICE` (section 10).

### Astronomy data

| File | What it is | Terms |
|---|---|---|
| `src/render/bsc5_stars.bin` | The Yale Bright Star Catalogue, 5th Revised Edition (Hoffleit, D. & Warren, W.H. Jr., 1991), quantised: 9,096 stars. Fetched from VizieR catalogue V/50, Centre de Données astronomiques de Strasbourg. | CDS asks that the catalogue be **cited**: "Hoffleit, D. & Warren, W.H. Jr. (1991), *The Bright Star Catalogue, 5th Revised Edition*. VizieR catalogue V/50, Centre de Données astronomiques de Strasbourg." No redistribution grant was located; see "Unverified" below and `src/render/PROVENANCE.md`. The star field is off by default. |

### Fonts (SIL Open Font License 1.1)

All under `assets/fonts/`, with the licence and every copyright statement in
`assets/fonts/LICENSE-OFL.txt`. All are subsets, which are Modified Versions in
the OFL's sense; the licence and these notices must travel with them.

| Files | Family | Copyright |
|---|---|---|
| `NotoSansJP-Regular.japan-subset.otf`, `NotoSansJP-Bold.japan-subset.otf` | Noto Sans JP | Copyright 2014-2021 Adobe (http://www.adobe.com/), with Reserved Font Name 'Source'. |
| `NotoSans-Regular.latin-subset.ttf`, `NotoSans-Bold.latin-subset.ttf` | Noto Sans 2.000 | Copyright 2015 Google Inc. All Rights Reserved. |
| `SourceSans3-Regular/Semibold/Bold.latin-subset.ttf` | Source Sans 3, 3.052R | Copyright 2023 Adobe (http://www.adobe.com/), with Reserved Font Name 'Source'. |
| `IBMPlexSansArabic-Regular/Bold.arabic-subset.ttf` | IBM Plex Sans Arabic 1.1.0 | Copyright 2019 IBM Corp. All rights reserved. |

The browser shell ships the Noto pairs; the Source Sans 3 and Plex Arabic
faces are compiled into the native binary for the story card
(`font_fallback::STORY_FONTS`). Elsewhere the engine shapes labels from the
operating system's own fonts and fetches no glyphs.

### Icons and sprite sheets

| Files | What it is | Terms |
|---|---|---|
| `web/sprites/maki/*.svg` | The Maki icon set, 215 SVGs, vendored unmodified | CC0 1.0 Universal, `web/sprites/maki/LICENSE.txt` |
| `web/sprites/japan-sprite@2x.png`, `.json`, `preview.png`; `ios/assets/japan-sprite@2x.png`, `.json` | A sprite sheet built by `build_sprites.py`: route shields, one-way arrows and a city dot drawn in this repository, plus the Maki icons | this repository's drawings, plus Maki under CC0 (`ios/assets/LICENSE-MAKI.txt`). See `web/sprites/README.md` and `ios/assets/PROVENANCE.md`. |

### Navigation text

| Files | What it is | Terms |
|---|---|---|
| `src/nav/osrm_text_instructions/en.json` | osrm-text-instructions 0.13.4, `languages/translations/en.json`, verbatim | BSD-2-Clause, Copyright (c) 2016, Project OSRM contributors; `LICENSE.txt` beside it and `NOTICE` section 8 |
| `tests/fixtures/osrm_text_instructions/en_v5_cases.json` | That project's English test fixtures, reduced | same; `README.md` and `LICENSE.txt` beside it |

### Test fixtures with their own licence

| Files | What it is | Terms |
|---|---|---|
| `tests/fixtures/cog/rainier_dem.tif` | Mount Rainier, resampled from the USGS 3D Elevation Program 1 arc-second DEM, tile n47w122 | U.S. public domain. Credit requested: "U.S. Geological Survey, 3D Elevation Program". `LICENSE-cog.txt` |
| `tests/fixtures/fgb/{alldatatypes,empty,heterogeneous,mp_overlapping,no_properties,poly00,unknown_feature_count}.fgb` | FlatGeobuf reference implementation test data, unmodified | BSD-2-Clause, Copyright (c) 2018, Björn Harrtell. `LICENSE-flatgeobuf.txt` |
| `tests/fixtures/geoparquet/naturalearth_countries.parquet` | The GeoParquet specification's `examples/example.parquet` (tag v1.1.0), unmodified; its data is five Natural Earth 1:110m countries (public domain) | Apache-2.0, `LICENSE-geoparquet.txt` |
| `tests/fixtures/orbit/stations.json`, `satcat_sources.tsv` | CelesTrak GP element sets for the `stations` group, and CelesTrak's SATCAT owner codes, both fetched 2026-09-29 | Data: US Space Force via CelesTrak. CelesTrak publishes it without a licence file and asks to be credited; terms in `demos/README.md`, "Satellite data (CelesTrak)". `tests/fixtures/orbit/PROVENANCE.md` |
| `tests/fixtures/orbit/vallado_subset.tle`, `vallado_subset.out` | Three SGP4 verification cases from Vallado et al., AIAA 2006-6753, via python-sgp4 | licence not recorded; see "Unverified" below |

### Digital twin vehicles (CC BY 4.0)

| Files | What it is | Terms |
|---|---|---|
| `examples/samples/data/digital-twin/vehicles/{cargovan,nypd,stepvan}.glb` | Valdrada's cargo van, NYPD SUV and step van by M. K. Turkcan, converted (`scripts/digital-twin-vehicles/`) | CC BY 4.0. Credit "Valdrada, M. K. Turkcan" |
| `examples/samples/data/digital-twin/vehicles/impala.glb` | Valdrada's CARLA-derived sedan, converted | CC BY 4.0. Credit "CARLA Simulator (carla.org)" |
| `examples/samples/data/digital-twin/vehicles/taxinv200.glb` | Valdrada's NYC cab on "Dhl delivery van" by memoov (Sketchfab), converted | CC BY 4.0. Credit "memoov" |

The twin draws "Vehicles: Valdrada (M. K. Turkcan), CARLA Simulator (carla.org), memoov · CC BY 4.0" on the map.
Changes are listed in `examples/samples/data/digital-twin/vehicles/SOURCE.md`.

### US Census boundaries

| File | What it is | Terms |
|---|---|---|
| `demos/race_dots/states.geojson` | US state outlines built by `scripts/gen-race-states.py` from the Census Bureau's `cb_2020_us_state_20m` cartographic boundary file | U.S. Census Bureau, public domain (17 U.S.C. 105) |
| `tests/fixtures/world_cities.csv`, and `tests/fixtures/fgb/world_cities.fgb`, `tests/fixtures/geoparquet/world_cities.parquet`, `studio-mobile/data/world_cities.parquet.geojson` written from it | 46 cities from Natural Earth 1:10m populated places v5.1.2 (`ne_10m_populated_places_simple`): name kept, position rounded to 0.01 degree, `pop_max` as `population`, `adm0cap` as `capital`. Made with Natural Earth. Free vector and raster map data @ naturalearthdata.com | Natural Earth, public domain |

### Made in this repository

These carry no third-party data, as recorded beside them:

- Every file named `*synthetic*` under `tests/fixtures/` and
  `swat-maps-doc/tests/fixtures/`, and `tests/fixtures/arrow/` (written by the
  crate's own tests).
- `examples/stories/data/port-route.geojson`: hand-drawn and illustrative
  (its own `note` property).
- `examples/stories/img/port-at-dawn.png`: credited "Illustration:
  self-made, CC0" by the story that uses it
  (`examples/stories/port-to-port-design.json`).
- `stress/*.json`, `tests/fixtures/*_golden.txt`, `tests/fixtures/stories/`:
  measurements and test expectations.

---

## Space look textures (`web/space/`)

`web/space/starmap_2020_3840.jpg` and `web/space/cloud_combined_2048.jpg`
are copies of upstream files, used by the optional space look and served by
the web demo. Stars: Deep Star Maps 2020 (https://svs.gsfc.nasa.gov/4851),
"NASA/Goddard Space Flight Center Scientific Visualization Studio. Gaia DR2:
ESA/Gaia/DPAC." Attribution required. Clouds: NASA Visible Earth, Blue
Marble: Clouds (https://visibleearth.nasa.gov/images/57747), NASA Goddard
Space Flight Center image by Reto Stoeckli. Both credits join the on-map
attribution while drawn. They are not in the published crates; the engine
downloads them on first use (`src/space_assets.rs`).

---

## Fetched at run time, not bundled

None of the following is in the repository. The examples, demos and scripts
download them on demand, most into `~/.cache/gpui-map*/`. If you ship a
product that fetches or bundles them, their terms apply to you.

| What | Used by | Licence and credit |
|---|---|---|
| OpenFreeMap planet vector tiles (OpenMapTiles schema) | `gpui_maps::basemaps`, most examples, the browser and iOS shells | OpenStreetMap data, ODbL 1.0. OpenMapTiles schema: code BSD-3-Clause, design CC BY 4.0. Credit: the OpenFreeMap TileJSON attribution, drawn by the engine. |
| Natural Earth II shaded relief (`ne2_shaded`, served by OpenFreeMap) | Liberty's relief layer at low zoom; the satellites demo's Earth | Natural Earth, public domain |
| OpenFreeMap sprite sheet `ofm_f384` | Liberty and the open look-alikes, through `gpui_maps::cached_sprite_atlas` | **Licence not independently verified** (`examples/assets/PROVENANCE.md`). `src/basemaps/PROVENANCE.md` records it as Maki (CC0) plus a public-domain arrow per openfreemap-styles' licence file. |
| Protomaps Florence demo archive (`pmtiles.io`) | `examples/pmtiles_remote.rs` default | Protomaps, OpenStreetMap contributors, ODbL |
| EOX Sentinel-2 cloudless 2016 | `basic_map` imagery (`GPUI_MAP_RASTER=s2cloudless`), `globe_capture --raster s2cloudless` | CC BY 4.0, EOX IT Services GmbH; contains modified Copernicus Sentinel data 2016 & 2017. Only the 2016 layer is used (later layers are CC BY-NC-SA). |
| US Census Bureau: TIGER/Line 2023 block groups, ACS 5-year 2023 (B19013), 2020 Census P.L. 94-171 and DHC, 2020 cartographic boundaries | `demos/us_block_groups.rs`, `demos/race_dots.rs`, `scripts/fetch-census-2020.sh`, `demos/data/fetch_us_block_groups.sh` | Public domain (U.S. Government works) |
| GHSL GHS-POP R2023A | `demos/world_dots.rs`, `scripts/fetch-ghsl.sh`, `tools/dotgrid-build` | European Commission, Joint Research Centre, CC BY 4.0 |
| CelesTrak GP element sets (`active`, `stations`, `gps-ops`, `geo`) | `demos/satellites.rs` (feature `orbits`) | Produced by the US Space Force, redistributed by CelesTrak (T.S. Kelso). Credit: "Orbits: CelesTrak / US Space Force". Usage policy in `demos/README.md`. |
| FlatGeobuf US counties demo file (GitHub Pages) | the FlatGeobuf panel (`kit/src/panels/fgb.rs`, `demos/fgb.rs`) | BSD-2-Clause, FlatGeobuf project |
| data.gov.sg datasets | `scripts/fetch-singapore-overlay.sh` | Singapore Open Data Licence v1.0, attribution required |
| Mapbox tiles, styles and sprites (**optional, off by default**) | the Mapbox-named examples, only with `GPUI_MAP_ALLOW_MAPBOX=1` and your own token | Mapbox Terms of Service; credit "© Mapbox © OpenStreetMap". Never committed. Without the opt-in, these examples draw an open look-alike. |

---

## Unverified: needs confirmation before release

The licence or origin of each item below is not recorded in the tree, or
was recorded as unverified. Each needs a decision (confirm, replace or drop)
before the public release.

- **`src/render/bsc5_stars.bin`, the Yale Bright Star Catalogue.**
  `src/render/PROVENANCE.md` states that no redistribution grant from CDS or
  Yale was located. The data is factual and CDS asks for citation, but
  redistributing a derived binary inside a compiled library has not been
  confirmed.
- **`tests/fixtures/routes/*.json`.** OSRM responses from a routing service,
  credited ODbL in `kit/src/kit.rs`. There is no `SOURCE.md` beside them
  naming the service, the request date or the road-network build.
- **`tests/fixtures/orbit/vallado_subset.{tle,out}`.** The provenance file
  names the source (Vallado et al., via python-sgp4) and hashes, but no
  licence.
- **`tests/fixtures/kepler_sg_hex_arc.json`, `swat-maps-doc/tests/fixtures/kepler/*.json`
  and `swat-maps-dash/tests/fixtures/bikeshare/station_usage.json`.** No provenance
  file. They look hand-written, but that is not recorded.
- **`ios/shots/*.png`.** Screenshots of rendered maps. Which tile source and
  style each shows is not recorded, so the credit they need is unknown.
- **`examples/satellite-style.json`.** Its `metadata` describes the design
  but does not say whether its label and boundary layers were written here
  or taken from Liberty. If taken from Liberty, Liberty's terms apply.
- **The OpenFreeMap sprite sheet** (fetched at run time, above).
- **The BSD-3-Clause copyright lines for the style code** (`NOTICE`,
  section 10), and the full ColorBrewer licence text (`NOTICE`, section 7).
  Neither was available locally to copy.

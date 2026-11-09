# demos

Showcases anyone can run from a fresh clone: no account, no key, no private
data. Each is one file and a Cargo example, using only the public API. See
`docs/superpowers/specs/2026-09-25-generic-demos-design.md`.

Basemap: Mapbox Light (`demos/mapbox-light-style.json`, vendored from
`mapbox://styles/mapbox/light-v11`) when `MAPBOX_ACCESS_TOKEN` is set,
otherwise OpenFreeMap Positron (`demos/positron-style.json`, vendored from
`https://tiles.openfreemap.org/styles/positron`, keyless) -- both pale greys
so a data overlay's own colours read clearly. Data comes from a script in
`demos/data/` and is never committed.

| demo | shows | data |
|---|---|---|
| `us_block_groups` | all 242,748 US Census block groups (70.7 M coordinates) as one runtime overlay, unsimplified, coloured by median household income on a continuous viridis `interpolate` ($2.5K-$250K, 75% fill, grey = no estimate), a gpui-drawn legend card, plus a headless `--measure` mode | `demos/data/fetch_us_block_groups.sh` (TIGER/Line 2023 + ACS 5-year 2023 B19013, public domain, ~1.1 GB) |
| `world_dots` | every person on Earth as a dot: the GHSL 30″ world (8.19 B people) through `MapView::set_dot_density_source`, with a counter (`N dots drawn · 1 dot = P people`), over Mapbox Streets (Positron without a token); `--synthetic` draws a seeded procedural world instead, and `--measure` prints the perf gate's `dots.*` lines | `scripts/fetch-ghsl.sh 2025 30ss` then `tools/dotgrid-build` (GHS-POP R2023A, CC BY 4.0, ~461 MB download, ~102 MB archive); none for `--synthetic` |
| `race_dots` | all 331,449,281 people of the 2020 Census as one dot per person, coloured by Kyle Walker's 8 race/ethnicity groups, over Mapbox Light (Positron without a token), with a legend (click a group to hide it), a view switcher, an About panel and the HUD (`people in view · dots · 1 dot = P people · level`); hexagon mode (H3 fill, `--mode hexagons`) and donut-cluster mode (`--mode clusters`) hand over to dots at z12; `--measure`/`--pan`/`--zoom-sweep` print MG-R11's frame stats, `--capture DIR` renders presets headless | `scripts/fetch-census-2020.sh --pl --dhc-dc` then `tools/race-build build` (2020 P.L. 94-171 + TIGER/Line, public domain, ~11.3 GB fetched but streamed -- see disk needs below, ~0.79 GB archive); `export-clusters` for cluster mode (~21.5 MB, no extra fetch); `load-clickhouse` + `export-hex`, or `--hexes-clickhouse`, for hexagon mode (needs a local ClickHouse, ~0.67 GB loaded, ~45.7 MB archive) |
| `satellites` (`--features orbits`) | every tracked satellite over the globe, propagated with SGP4: group chips (Stations, Starlink, GPS, GEO, other), hover card, click to select (orbit ring, ground track, Follow), the ISS with its halo, ring and track, a time bar (play, speeds to 1000x either way, ±24 h scrubber, Now), Show GEO / Show LEO and an About card; `--no-fetch` draws the cache as it is, `--synthetic` a seeded set, `--capture DIR` renders five presets headless, `--fetch-only GROUP` runs the fetch policy once | CelesTrak GP data, fetched by the demo itself into `~/.cache/gpui-map-data/satellites` (see *Satellite data* below); `active` ~4-6 MB, `stations` ~9 KB |

`us_block_groups` is also in the Studio (`cargo run --example studio`) as
`--open census`; `race_dots` as `--open race-map`; `satellites` as `--open satellites`
(`--features orbits`); `world_dots` is also in the Studio as `--open world-dots`
(and, behind the off-by-default `clickhouse-demo` feature, `--open
world-dots-clickhouse`).

```sh
demos/data/fetch_us_block_groups.sh              # into ~/.cache/gpui-map/demos/us-block-groups
cargo run --release --example us_block_groups -- --states 44 --measure
cargo run --release --example us_block_groups -- --all --no-outline
cargo run --release --example us_block_groups -- --all --basemap positron
```

```sh
cargo run --release --example world_dots -- --synthetic --measure         # no data, no window
cargo run --release --example world_dots -- --at india                   # needs the 30″ archive
cargo run --release --features probe --example world_dots -- --at dhaka --zoom-sweep
cargo run --release --example world_dots -- --level-switch-ab ~/.cache/gpui-map-data/ab
```

```sh
scripts/fetch-census-2020.sh --pl --dhc-dc                              # 51 P.L. 94-171 zips + DC's DHC zip, no key
cargo run --release --manifest-path tools/race-build/Cargo.toml -- build   # streams all 51 states' TIGER zips, keeps none
cargo run --release --example race_dots -- --at us --measure
cargo run --release --example race_dots -- --at manhattan
```

```sh
# optional: cluster mode's archive (no ClickHouse; reads the build's own blocks.points)
cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-clusters
cargo run --release --example race_dots -- --mode clusters --at houston

# optional: hexagon mode's archive (needs a local ClickHouse at localhost:8123)
cargo run --release --manifest-path tools/race-build/Cargo.toml -- load-clickhouse
cargo run --release --manifest-path tools/race-build/Cargo.toml -- export-hex
cargo run --release --example race_dots -- --mode hexagons --at houston
```

`race_dots` reads `~/.cache/gpui-map-data/census2020/race-dots-2020.pmtiles`
unless `--dots FILE` names another archive (built by `race-build build`,
above); hexagon and cluster mode read `race-hex-2020.pmtiles` and
`race-clusters-2020.pmtiles` the same way, or `--hexes-clickhouse URL`
reads hexagons live (needs the `clickhouse-demo h3` features). Presets
(`--at`): `us`, `manhattan`, `la`, `houston`, `dc`; `--view LON,LAT,ZOOM`
takes any camera. `--hide G,G,..` starts with groups 0-7 (the legend's
order: Hispanic, White, Black, AIAN, Asian, NHPI, other, two or more)
toggled off.

**Disk (spec §2.5, `docs/superpowers/specs/2026-09-27-us-race-dot-map-design.md`):**
every step checks free space first and refuses under its own need plus
10 GB. `race-build build` streams TIGER/Line one state at a time (at most
2 zips in flight, each deleted right after its state converts, peak
≤ 2.5 GB above baseline) rather than keeping all 51 (10.3 GB); the P.L.
zips are kept (1.33 GB); the converter's own spill files reach 3.3 GB
(`parts.spill` + `blocks.points`, deleted once the archives are built);
and the three output archives together are under 1 GB (measured: dots
0.79 GB, hexagons 45.7 MB, clusters 21.5 MB). Nothing under
`~/.cache/gpui-map-data/census2020/` is ever `git add`-ed.

`world_dots` reads `~/.cache/gpui-map-data/ghsl/E2025_30ss.pmtiles` unless
`--grid PATH|URL` names another archive; when the file is missing it prints
the fetch and build commands and draws the synthetic world, labelled
"synthetic" on screen. Presets (`--at`): `world` z1, `india` z5, `dhaka` z10,
`dhaka15` z15. `--capture DIR` renders each preset headless over Mapbox
Streets to a PNG.

`us_block_groups` joins ACS 2023 median household income (B19013_001E) onto
each block group by GEOID, from `acs/income_bg_2023.csv` next to the
shapefiles. The fetch script asks the Census API once per state when
`CENSUS_API_KEY` is set (the API now refuses keyless requests), and otherwise
reads the same table from the Census summary-file download (~18 MB, no key).
Negative Census sentinels (e.g. -666666666) become "no data" and draw grey.
The paint is a plain style expression the overlay compiles:
`["case", ["has", "income"], ["interpolate", ["linear"], ["get", "income"], ...], "#9e9e9e"]`,
quantised by the engine to at most 256 ramp colours so the paint classes stay
at a few hundred. `--geojson FILE` carries no income, so it draws all grey.

The windowed run also draws a white legend card, top-left, over the map --
title, a continuous viridis gradient bar, tick labels at the ramp's $2.5K/
$64.37K/$126.25K/$188.13K/$250K stops, and a "2.5K — 250.0K" caption. It is
built from ordinary gpui elements inside the demo (`legend()` in
`demos/us_block_groups.rs`), layered over the map via
`MapView::set_chrome_above_map`, not an engine feature. `--basemap
mapbox|positron` overrides the token-presence basemap pick either way. Kyle
Walker's mapgl reference this restyle follows also draws basemap place
names on top of the choropleth; that needs an engine change to the layer
draw order and is not done here.

### Satellite data (CelesTrak)

The `satellites` demo downloads GP (OMM JSON) element sets from CelesTrak,
`https://celestrak.org/NORAD/elements/gp.php?GROUP=<group>&FORMAT=json`,
for the groups `active`, `stations`, `gps-ops` and `geo` only.

- **Source and credit.** The element sets are produced by the US Space
  Force (published on space-track.org)
  and redistributed by CelesTrak (T.S. Kelso). The map credit is
  "Orbits: CelesTrak / US Space Force", appended after the basemap's own.
- **Terms.** CelesTrak publishes the data free of charge and without a
  licence file; it asks that it be credited and used considerately. Its
  usage policy: the GP data updates about every two hours, a client must
  not ask for the same group more often than that, a repeat request
  before new data may be answered `403`, and abusive clients are blocked.
- **What the demo does about it** (`demos/satellites/fetch.rs`): at most one
  request per group per two hours whatever the outcome, no retries, stop on
  any non-200 and keep the cached copy, stop altogether after 3 errors in
  a row until the user asks again, one fetcher at a time (a lock file),
  and a User-Agent naming this project
  (`gpui-map-satellites/<version> (+https://github.com/swatmobility/gpui-maps)`).
  Tests never contact CelesTrak; they run against a local stub.
- **Never committed.** Fetched data lives in `~/.cache/gpui-map-data/satellites`.
  The one exception is the 22-record test fixture
  `tests/fixtures/orbit/stations.json`, whose provenance is in
  `tests/fixtures/orbit/PROVENANCE.md`.

```sh
cargo run --release --features orbits --example satellites -- --fetch-only stations   # one request at most
cargo run --release --features orbits --example satellites                            # the window; refreshes due groups once
cargo run --release --features orbits --example satellites -- --no-fetch --capture /tmp/sats
cargo run --release --features orbits --example studio -- --open satellites
```

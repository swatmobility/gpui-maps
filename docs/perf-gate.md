# The census performance gate

`scripts/perf-gate.sh` turns the US block groups benchmark (`demos/us_block_groups.rs`: 242,748 polygons as one
tiled overlay source) into a pass/fail check, so the gains recorded in
`docs/measurements/2026-09-25-us-block-groups-breakdown.md` and
`docs/measurements/2026-09-26-overlay-zoom-continuity.md` cannot erode without someone noticing.

```text
scripts/perf-gate.sh                 # headless tier, ~10 s after the build
scripts/perf-gate.sh windowed        # windowed tier, ~4 min, needs the screen
scripts/perf-gate.sh all
scripts/perf-gate.sh all --reps 3    # grade the p50 of 3 runs per mode
```

## When to run it

Run it before merging any change to rendering, overlays or the basemap. That covers `src/render/`,
`src/overlay/`, `src/map_view.rs`, and anything that changes tile baking, caching or upload.

- The headless tier is quick and deterministic enough to run unattended.
- Run the windowed tier when a change can affect what a frame costs.

## What it runs

Every run uses the national dataset as one source (`--all --one-source`) on the keyless Positron basemap, with the
`probe` feature on. The demo prints `gate: NAME VALUE UNIT` lines when `US_BG_GATE` is set. `examples/perf_gate.rs`
parses those lines and nothing else, so rewording a human-readable row cannot change what is graded.

### Headless tier (`--measure`, no window)

| Metric | What it is | Band |
|---|---|---|
| `headless.push_ms` | Synchronous `set_source_columns` | time |
| `headless.index_build_ms`, `index_mb` | The tile index that lands off-thread | time / bytes |
| `headless.property_mb`, `store_features_mb` | `probe_census` property and feature-vec bytes | bytes |
| `headless.rss_at_rest_mb`, `heap_at_rest_mb`, `heap_peak_mb`, `peak_rss_mb` | Memory once the index has landed, and the peaks | bytes |
| `headless.cover.{national,dc_z8,la_z11}.bake_wall_ms` | Cover bake at three fixed 1280×800 cameras | time |
| `headless.cover.*.{vertices,indices,outline_indices}` | What those bakes produce | **exact, both sides** |
| `headless.path.wanted_tiles`, `path.unbaked_with_content` | z3..z12 over DC and LA: tiles wanted, and wanted tiles whose cut has features but whose bake produced nothing | exact / must stay 0 |
| `headless.classify_ms` | Paint-change reclassification, best of 3 | time |
| `headless.table_only.{p50,max}_ms`, `table_only.other_path` | 40 snapped slider ranges through `update_source_paint`; every one must take the table-only path | time / must stay 0 |
| `headless.hit_mean_us`, `hit_hits` | `feature_at` plus properties over a 64×48 grid | time / exact |
| `headless_states.table_only.{p50,max}_ms`, `table_only.other_path` | The same 40 slider ranges on the demo's DEFAULT shape, one source per state (`--measure --states 11,06,48`: DC is under the tiling threshold, CA and TX over it). Its own mode, because the one-source run above cannot see a small state that replans on every drag step -- which is how that shipped with the slider and stayed green until 2026-09-27 | time / must stay 0 |

### Headless tier, second section: `points` (`demos/big_points.rs --measure`)

2 M seeded synthetic points (no data to fetch) on the point path (`set_source_cluster_options`), radius
clustering with two aggregates. Its bands live in the same baseline file under the `points.` prefix.

| Metric | What it is | Band |
|---|---|---|
| `points.push_ms` | Synchronous `set_source_points` | time |
| `points.index_build_ms`, `index_{project,sort,levels}_ms` | The radius-mode index build and its phases | time |
| `points.grid_index_build_ms`, `lod_index_build_ms` | The grid roll-up and the non-clustered level of detail on the same points | time |
| `points.data_mb`, `index_mb`, `grid_index_mb`, `lod_index_mb`, `store_resident_mb`, `rss_at_rest_mb`, `peak_rss_mb` | Memory | bytes |
| `points.view.{national_z3,city_z8,city_z11,city_z14,city_z17}.{features,instances}` | The view at five fixed cameras: features, and the instanced circles it draws | **exact** |
| `points.view.{...}.upload_mb` | What the view uploads: 16-byte instances plus its paint table | bytes |
| `points.zoom_change_worst_ms` | Worst view build (instances included) over z0..z18 above the biggest city | time |

Run or re-baseline only this section with `--only points`:

```text
scripts/perf-gate.sh headless --only points
scripts/perf-gate.sh headless --update-baseline --reps 3 --only points
```

### Headless tier, third section: `dots` (`demos/world_dots.rs --measure --synthetic`)

The dot-density layer (`MapView::set_dot_density_source`, see
`docs/superpowers/specs/2026-09-26-world-dot-density-design.md`) over a **procedural synthetic world** -- the gate
never fetches or reads the GHSL archive, `--synthetic` is always passed, and no data file is ever needed. Its bands
live in the same baseline file under the `dots.` prefix.

Dasymetric placement (`docs/superpowers/specs/2026-09-27-dasymetric-dot-placement-design.md`) relocates a dot inside
its own cell onto a building footprint when one is nearby, by the shipped rulings: relocation runs from `z >= 15`
only (Streets carries no buildings below that), on a flat camera only (`pitch <= 30 deg`; steeper pans keep the
uniform placement for stability), with `K = 8` candidates and a mask ring capped at `32 MB` (bytes only, no tile-count
cap; demoted first past the cap); a tile-less mask texel gets a NO-DATA rank rather than a guess, and `DotStyle`'s
overlap fade uses `overlap_gamma = 0.35`. None of this changes what is alive at a given ppd (DP-1, graded below).

| Metric | What it is | Band |
|---|---|---|
| `dots.view.{world_z1,india_z5,dhaka_z10,dhaka_z15}.{cells,blocks}` | Populated 30″ cells and the dot blocks they decompose into, at four fixed cameras | **exact** |
| `dots.view.{...}.dots` | Dots alive at the view's settled ppd (`vs_dot`'s own alive rule) | **exact** |
| `dots.view.{...}.upload_mb` | 16-byte `DotBlock`s uploaded for the view | bytes |
| `dots.tile_build_ms_p50`, `tile_build_ms_max` | `build_blocks` per drawn tile, over the four views | time |
| `dots.zoom_change_worst_ms` | Worst of the plan plus every drawn tile's block build, z0..z16 at Dhaka (the fetch is IO and not counted; noisy on a busy build worker, so this is a Time band, not exact) | time |
| `dots.nesting_violations` | Task 2's sampled check (a dot alive at `ppd` must be alive at `ppd/2`) over real cells | **must stay 0** |
| `dots.world_total_dots` | Every level-0 cell's dots at ppd 1, summed over the whole synthetic world | **exact** |
| `dots.world_total_err_pct` | `\|dots - people\| / people` at ppd 1, over the whole world | **absolute cap 0.1, independent of the measured baseline** (currently ~0.00008) |
| `dots.peak_rss_mb` | Peak RSS of the `--measure` process | bytes |
| `dots.relocate.{world_z1,india_z5,dhaka_z10,dhaka_z14,dhaka_z15}.count_diff` | DP-1: each of the five views settled twice (relocation on, mask `k >= 2`, against a synthetic checkerboard mask; and off, `hash::dot_alive` alone) -- `DotStats`, the plan's draws and every drawn tile's `(live, digest)` pair compared. The count of differences | **must stay 0** (Policy::NoMore) -- the one rule (a dot may move only inside its own cell) never changes what is alive |
| `dots.relocate.cpu_mirror_ms` | A harness proxy, not a mask pass: the CPU mirror's cost per drawn tile (`MaskGrid::plan` + the synthetic texel fill + `alive_digest`'s relocating branch), median over the five views' tiles. Headless there is no GPU mask pass to time, and this row times none of the real one -- the window's pass is timed by `MaskPassTimer` (probe builds, `TIMESTAMP_QUERY`; DP-3 in `docs/measurements/2026-09-27-dasymetric-dot-placement.md`). It guards the mirror (and `MaskGrid::plan`) against getting slower. Named `dots.relocate.mask_pass_ms` until the dasymetric final fix wave | time |

Run or re-baseline only this section with `--only dots`:

```text
scripts/perf-gate.sh headless --only dots
scripts/perf-gate.sh headless --update-baseline --reps 3 --only dots
```

A hole is something that happens at paint time: the pick found nothing to draw for a tile. A headless run cannot
see that. What it can see is the bake half of a hole: a wanted tile with content that baked to nothing. That is
`path.unbaked_with_content`. The windowed tier counts real holes.

### Headless tier, fourth section: `sats` (`demos/satellites.rs --measure --synthetic`)

The satellites layer (`docs/superpowers/specs/2026-09-29-satellites-design.md` §5.4) over the **seeded synthetic
catalogue** (`orbit::synthetic::synthetic_records(7)`, the demo's own `--synthetic`): exactly 10,000 objects -- 7,000
at 550 km and 53° (a Walker shell), 1,500 at 500-1,200 km and 97°, 31 GPS-like, 500 GEO, and 969 Molniya/GTO -- all
at one fixed epoch (`SYNTHETIC_EPOCH_UNIX`), measured at a fixed scene time one hour after it. It needs **no network
and no data**: the section never asks CelesTrak and never reads a cache, so `--only sats` skips the census data
check. The demo is built `--features probe,orbits`. Its bands live in the same baseline file under the `sats.` prefix.

| Metric | What it is | Band |
|---|---|---|
| `sats.keyframe_ms` | One full keyframe, `KeyframeBuilder::new(t, 10_000).step(&CatalogSource, usize::MAX)`, single thread, best of 20 after 250 ms of untimed keyframes (a process launched after the machine idled runs its first ~40 ms ~1.4x slow; a one-keyframe warm-up timed that ramp, not sgp4) | time |
| `sats.propagate_ns` | `keyframe_ms` per object | time |
| `sats.hit_us` | `hit_test` over a 64×48 grid at 1440×900, z -1, lat 20, on the layer's keyed instances; the median of 5 passes' per-test mean | time |
| `sats.parse_ms` | `parse_omm` + `Catalog::build` of the synthetic records serialised as a CelesTrak-shaped OMM JSON body (~4 MB), best of 5 | time |
| `sats.visible.{z-1_lat0,z-1_lat60,z3_lat0}` | Instances `AltitudeFrame::position` draws (in front of the near plane, not hidden by the planet) at three fixed cameras, 1440×900, lon 0 | **exact** |
| `sats.upload_kb` | One keyframe upload: `⌈instances × 60 B / 1024⌉` | **exact** |
| `sats.dots_px_s.{z-1_lat0,z-1_lat60,z3_lat0}` | The fastest on-screen dot's speed bound, device px per scene second, on a 2x display at the three cameras (`AltitudeFrame::dot_motion`) | **±5%** (hand-set band: under- and over-estimates both fail) |
| `sats.dots_redraw_hz.{z-1_lat0,z-1_lat60,z3_lat0}` | Frames a second the moving dots ask the host for at rate 1 (`1 / dot_redraw_after_s(1, dot_motion)`: a quarter pixel moved, an off-screen dot entering, an object appearing or vanishing). Each is a whole-window gpui redraw; before the bound it was every display refresh (120 Hz on ProMotion) | **no more** |
| `sats.dots_speed_us` | That bound's per-paint cost: `dot_motion` over the 10k keyed instances at z3, the median of 7 | time |

**Why the visible counts are exact.** The set, the scene time and the cameras are all fixed, so the count of dots
in front of the planet is a pure function of the occlusion and near-plane tests. A broken occlusion (the soft edge's
sign flipped, `eps` wrong, the eye in the wrong frame, the TEME rotation off) changes a count, not a time -- it would
never show up in a timing band -- so any change at all fails. The same holds for the upload: 60 bytes per instance
of a fixed 10,000.

Run or re-baseline only this section with `--only sats`:

```text
scripts/perf-gate.sh headless --only sats
scripts/perf-gate.sh headless --update-baseline --reps 3 --only sats
```

The baseline rows are written by the controller's first `scripts/perf-gate.sh headless --update-baseline` run over
this section, not by hand.

### Headless tier, CSV section: `csv` (`demos/csv_points.rs --measure`)

A seeded synthetic CSV of 1,000,000 rows (~51 MB: `id,lat,lon,category,value,name,flag`, every 1000th row missing
its longitude) parsed by `overlay::csv::CsvSource::from_bytes`, pushed with `OverlayStore::set_source_csv` onto the
point path, and indexed. No network and no data, so `--only csv` skips the census data check. Its rows are not
baselined yet: they print as ungraded until the first `--update-baseline --reps 3 --only csv`.

| Metric | What it is | Band |
|---|---|---|
| `csv.parse_ms` | `from_bytes`, best of 3 | time |
| `csv.rows_loaded`, `csv.rows_skipped` | 999,000 and 1,000 | **exact** |
| `csv.source_mb` | The parsed `CsvSource` (positions + typed columns) | bytes |
| `csv.push_ms`, `csv.index_build_ms` | Synchronous `set_source_csv`; the point index | time |
| `csv.store_resident_mb`, `csv.peak_rss_mb` | Memory | bytes |
| `csv.view.national_z4.features` | The point view at z4 over the contiguous US | **exact** |

### Headless tier, Explorer section: `explorer` (`demos/explorer_gate.rs --measure`)

The Studio Explorer's filter path (plan Task 3.6, spec §11.4) on a seeded synthetic million-row quakes CSV
(`time`, `latitude`, `longitude`, `depth`, `mag`, `rms`, `magType`, `tsunami`). It runs the Explorer's own pieces:
`overlay::csv` plus `swat_maps_doc::fields::refine`, the binding's push (time as milliseconds), a GPU filter declared
on `mag`, and a CPU filter's mask, `Table::take` and re-push. No window: a "frame" here is the point view a pan step
builds. Re-baseline only this section with `--only explorer`.

| Metric | What it is | Band |
|---|---|---|
| `explorer.load_ms`, `push_ms` | Parse and type the CSV; the synchronous push | time |
| `explorer.rows_loaded` | Rows the seeded file loads | exact |
| `explorer.points_1m.pan_frame_ms_p50` | The point view one pan step builds at z5, p50 over 24 steps | time |
| `explorer.filter_drag.uploads` | Point-data revisions and index rebuilds over 40 drag steps of a GPU window | must stay 0 |
| `explorer.filter_drag.count_ms_p50` | The drawn-row count a drag step asks for off the UI thread (`PointFilter::passes_all`) | time |
| `explorer.cpu_filter_release_ms_p50` | A CPU filter's release: mask, passing rows, re-push; p50 of five windows | time, never above 250 ms (E-mask is owed above it) |
| `explorer.cpu_filter_index_ms_p50` | The index that re-push builds off the UI thread | time |

### Headless tier, MBTiles section: `mbtiles` (`demos/mbtiles.rs --measure`)

A seeded synthetic MBTiles file: 1024 z14 tiles, each one of the three committed OpenFreeMap tiles
(`benches/fixtures/`) stored gzipped (~0.2 GB, written to the temp dir and deleted), read through
`MbtilesSource`'s `TileSource::fetch` (tokio blocking pool, SQLite, gunzip) after one warm-up pass. No network and
no data. Its rows are not baselined yet: they print as ungraded until the first `--update-baseline --reps 3 --only
mbtiles`. `demos/mbtiles.rs --measure PATH` times a real file the same way (not graded).

| Metric | What it is | Band |
|---|---|---|
| `mbtiles.open_ms` | `MbtilesSource::open` (metadata read), best of 3 | time |
| `mbtiles.fetch_p50_us`, `mbtiles.fetch_p95_us` | One fetch at a time, shuffled order, warm | time |
| `mbtiles.concurrent_p50_us`, `mbtiles.concurrent_p95_us` | 32 fetches in flight | time |
| `mbtiles.tiles_read` | 1024 | **exact** |

### Headless tier, FlatGeobuf section: `fgb` (`demos/fgb.rs --measure`)

A seeded synthetic FlatGeobuf of 1,000,000 points (~171 MB: `id`, `category`, `value`, `name`, clustered over the
contiguous US) written by `overlay::fgb::write` with a node-size-16 index to the temp dir (deleted afterwards), read
whole by `FgbSource::from_path`, pushed with `OverlayStore::set_source_fgb` onto the point path, then queried by
bbox through the index by seek (warm page cache). No network and no data. Its rows are not baselined yet: they
print as ungraded until the first `--update-baseline --reps 3 --only fgb`.

| Metric | What it is | Band |
|---|---|---|
| `fgb.load_ms` | `from_path` (read + decode every feature), best of 3 | time |
| `fgb.features_loaded` | 1,000,000 | **exact** |
| `fgb.file_mb`, `fgb.source_mb` | The file; the read `FgbSource` (positions + typed columns) | bytes |
| `fgb.push_ms` | Synchronous `set_source_fgb` | time |
| `fgb.query.city.p50_us`, `.p95_us` | 100 bbox reads of ~1 x 0.6 degree at seeded centres | time |
| `fgb.query.region.p50_us`, `.p95_us` | 100 bbox reads of ~8 x 5 degrees | time |
| `fgb.query.fixed.features` | The features in one fixed box: the index's answer | **exact** |
| `fgb.peak_rss_mb` | Memory | bytes |

### Headless tier, GeoParquet section: `geoparquet` (`demos/geoparquet.rs --measure`)

A seeded synthetic GeoParquet of 1,000,000 points (~73 MB snappy: `id`, `category`, `value`, `name`, the `fgb`
section's cluster model), Z-order sorted and written by `overlay::geoparquet::write` with a 1.1 bbox covering column in
31 row groups of 32,768 rows to the temp dir (deleted afterwards), read whole by `GeoParquetSource::from_path`, pushed
with `OverlayStore::set_source_geoparquet` onto the point path, then read by bbox (row groups skipped by their covering
statistics, rows filtered by the covering column; warm page cache). No network and no data. Ungraded until the first
`--update-baseline --reps 3 --only geoparquet`.

| Metric | What it is | Band |
|---|---|---|
| `geoparquet.load_ms` | `from_path` (every column chunk, decoded), best of 3 | time |
| `geoparquet.load_geometry_only_ms` | The same with no property columns (projection pushdown) | time |
| `geoparquet.rows_loaded` | 1,000,000 | **exact** |
| `geoparquet.file_mb`, `geoparquet.source_mb` | The file; the read `GeoParquetSource` | bytes |
| `geoparquet.push_ms` | Synchronous `set_source_geoparquet` | time |
| `geoparquet.query.city.p50_us`, `.p95_us` | 100 bbox reads of ~1 x 0.6 degree at seeded centres | time |
| `geoparquet.query.region.p50_us`, `.p95_us` | 100 bbox reads of ~8 x 5 degrees | time |
| `geoparquet.query.fixed.rows_loaded` | The rows in one fixed box | **exact** |
| `geoparquet.query.fixed.row_groups_read` | The row groups that box reaches (12 of 31): pushdown, counted | **exact** |
| `geoparquet.peak_rss_mb` | Memory | bytes |

### Headless tier, Cloud GeoTIFF section: `cog` (`demos/cog.rs --measure`)

A seeded synthetic pair written by `tile_source::cog::write` to the temp dir (deleted afterwards): a 4096 x 4096 RGB
COG (deflate, horizontal predictor) and a 4096 x 4096 float32 DEM (deflate, floating-point predictor, a nodata square,
GDAL statistics), both 256 px internal tiles with four overviews, over one z6 Web Mercator tile. No network and no data
(the HTTP rows use a local server that delays every response 20 ms).

| Metric | What it is | Band |
|---|---|---|
| `cog.open_us` | `CogSource::open` of the local file (the header read and parse), best of 5 | time |
| `cog.tile.rgb.p50_us`, `.p95_us` | One 512 px map tile, cold (every internal tile it needs decoded), 40 seeded tiles at z7-z9 | time |
| `cog.tile.dem.p50_us`, `.p95_us` | The same on the DEM, terrain ramp and hillshade | time |
| `cog.tile.dem_flat.p50_us`, `.p95_us` | The same without the hillshade | time |
| `cog.view.map_tiles` | The 1280 x 800 view at z8 the next rows read: its map tiles | **exact** |
| `cog.view.tiles_decoded` | The internal tiles that view decodes (the overview choice, counted) | **exact** |
| `cog.view.coalesced.range_requests` | The range requests it costs with the default 32 KiB merge gap | **exact** |
| `cog.view.uncoalesced.range_requests` | The same with coalescing off: one request per internal tile | **exact** |
| `cog.view.http_1.ms`, `cog.view.http_8.ms` | The view's map tiles all requested at once over HTTP (20 ms per response), one / eight requests in flight per map tile | time |

### Headless tier, night race section: `night_race` (`demos/night_race_gate.rs --measure`)

The night race's in-car view as the page draws it in the car: the player's own car and the driver (with the
cockpit's insides) as two model layers in the engine's first-person near pass (`render::model::ModelNearPass`), their
part tables moved every frame by the steering rig (the wheel sawing between 60° left and right), at the in-car camera
(0.86 m up, pitched 75°, a 58° FOV) into a 2560 x 1600 target. The scene behind is a cleared pass: these are the model
pass's own costs. No network and no data.

| Metric | What it is | Band |
|---|---|---|
| `night_race.in_car.frame_p50_ms`, `.frame_p95_ms` | 240 frames: prepare (uniforms, part tables), encode, submit to idle | time |
| `night_race.in_car.model_pass_gpu_p50_us`, `_p95_us` | The near pass's GPU time, a two-stamp `TIMESTAMP_QUERY` set round it (devices with the feature) | time |
| `night_race.in_car.triangles` | Triangles the near pass draws (both layers) | **exact** |
| `night_race.in_car.parts_upload_bytes` | Part-table bytes a frame writes (one table per layer with parts) | **exact** |

```
scripts/perf-gate.sh headless --only night_race
```

### Headless tier, trips section: `trips` (`demos/trips.rs --measure`)

20,000 seeded synthetic flights between 60 hubs, great circles sampled once a minute inside one hour (1.1M vertices),
drawn on the globe at the world-flights sample's camera into a 2560 x 1600 target as the Studio page draws them: the
paths whole (hairline) and the fading trails over them, one set shared by both layers. No network and no data.

| Metric | What it is | Band |
|---|---|---|
| `trips.build_ms` | `TripSet::from_trips` (CPU, once per data set) | time |
| `trips.flights.trips`, `.vertices`, `trips.upload_kb` | The seeded set: trips kept, vertices, bytes uploaded once | **exact** |
| `trips.push_ms` | The first frame: the upload plus the draw, submit to idle | time |
| `trips.globe.frame_ms` | A steady frame (median of 30), the clock one playback step later each time | time |
| `trips.globe.draws` | Draws per frame (one per layer) | **exact** |
| `trips.tick_cpu_us` | `TripPipeline::prepare` for a frame where only the clock moved: the per-tick CPU cost | time |
| `trips.tick_upload_bytes` | Vertex bytes uploaded over those 30 ticks: a clock move is a uniform write | **no more** (0) |

Run or re-baseline only this section with `--only trips`:

```
scripts/perf-gate.sh headless --only trips
scripts/perf-gate.sh headless --update-baseline --reps 3 --only trips
```

### Headless tier, vector-tile overlay section: `vtoverlay` (`demos/vtoverlay.rs --measure`)

Seeded synthetic MVT tiles shaped like the two Kepler phase-7 samples: four z10 tiles of 3,000 census-block squares
(a `fill` layer with Kepler's stepped `total_pop` colours, at the population sample's camera, z12.35) and twelve z14
tiles of 6,000 places (a `circle` layer coloured by postcode, at the FSQ sample's camera, z14.1). No network, no data.

| Metric | What it is | Band |
|---|---|---|
| `vtoverlay.{population,places}.decode_ms` | `vector_tiles::tile_features` over the view's tiles: MVT decode, clip, paint-case match | time |
| `vtoverlay.{population,places}.features` | Features after clipping (blocks; places) | **exact** |
| `vtoverlay.{population,places}.push_ms` | The push into an overlay store as `MapView::push_vector_overlay` builds it (circles as a point source) | time |
| `vtoverlay.{population,places}.frame_ms` | A frame of the view through the capture renderer, 1280 x 800, no basemap (median of 5; the capture re-reads its GeoJSON overlay each render, so this includes that parse and bake) | time |

```
scripts/perf-gate.sh headless --only vtoverlay
```

### Headless tier, fifth section: `story` (`examples/story.rs tests/fixtures/stories/det.json --gate`)

Story content 1a (`docs/superpowers/specs/2026-09-30-story-content-1a-design.md` §8.9). The determinism story
`tests/fixtures/stories/det.json` (Marina Bay, a jump, a fly and an orbit, a caption) on the **vendored Singapore
fixture tile** (`story::fixture::FixtureSource`), offline, rendered to a null sink three times. It needs **no
network and no data**, so `--only story` skips the census data check; it writes no file.

The `draw` and `card` runs are the design-system path (story design spec
`docs/superpowers/specs/2026-10-03-story-design-system-design.md` §14.3): det with `design: {}` and a three-vertex
`role: "route"` line with no paint (`line_metrics`), themed into its ahead dots, casing and core with the draw head,
`draw`n linearly from 0.2 s to 1.8 s. The `card` run adds an event card (a headline and two stats) at 9:16 1080p, so
the frame also carries the card plate, the fade and the fine print.

| Metric | What it is | Band |
|---|---|---|
| `story.uhd.frame_ms_p50` | The p50 of one frame's render time (the sum of its `story::STAGES`: plan, wait, bake, encode, readback, burn-in, write), the script at 16:9 4K (3840×2160, pixel ratio 2), 20 frames | time |
| `story.draw.frame_ms_p50` | The same p50 at 16:9 1080p, the camera the script's opening jump then a 2 s orbit, with the themed route `draw`n (3 sublayers and the head) | time |
| `story.draw.retessellations_after_first` | `Capture::story_retessellations` summed over every frame of that run but the first | **no more** (baseline 0, so 0) |
| `story.card.frame_ms_p50` | The same p50 for the `card` run at 9:16 1080p (1080×1920) | time, **ceiling never above 4.0 ms** (spec budget) |
| `story.card.reshapes_after_first` | `Capture::story_reshapes` summed over every frame of the `card` run but the first: label shapes of a key the session had shaped before and trimmed since | **no more** (baseline 0, so 0) |
| `story.card.layout_us_p50` | The p50 of `Capture::chrome_layout_us` (the frame's `chrome::layout`: corner, card, fine print, measured through the warm shape cache) over every `card` frame but the first, µs | time, **ceiling never above 300 µs** (spec budget) |
| `story.uhd.frames`, `story.draw.frames`, `story.card.frames` | Frames rendered (20 each) | context, never graded |

A run that renders no frame prints no row: `--gate` fails instead.

The budget caps hold at grade time too: a committed band whose ceiling passes 4.0 ms or 300 µs is graded at the budget.
On a busy machine (load above the gate's threshold) every failure, these included, is advisory, so a budget breach
seen only under load must be re-run quiet before it counts.

**Owed:** the `story.draw.*` band in the baseline predates the themed-route scene (it was taken on the plain route,
2026-10-01); until the owed quiet-machine `--update-baseline --only story` lands, that row is graded against the old
scene's band and the `story.card.*` rows are not graded at all.

**Why the draw run orbits instead of flying.** A `draw` is a trim uniform written every frame, so after the first
frame tessellates the layer, no frame may tessellate it again. A camera that crosses an integer zoom does re-resolve
the layer's paint and re-tessellate it, once, by design (`capture::story_overlays`); det's own fly (z14.2 to z15) would
add exactly that 1 and hide the row's question. The orbit moves every frame at one zoom. Every frame must also draw a
tile, or the run fails rather than reporting a figure from a blank frame.

Run or re-baseline only this section with `--only story`:

```text
scripts/perf-gate.sh headless --only story
scripts/perf-gate.sh headless --update-baseline --reps 3 --only story
```

The baseline rows are written by the controller's first `--update-baseline` run over this section, not by hand.
Measured by Task 22 (three `--gate` runs, release, 2026-10-01): `story.uhd.frame_ms_p50` 10.4-10.9 ms,
`story.draw.frame_ms_p50` 2.8-2.9 ms, `story.draw.retessellations_after_first` 0.

### Headless tier, space look section: `look` (`demos/satellites.rs --measure-look`)

The opt-in space look (`render::space_look`, on by default in the satellites scene): what its two full-screen
passes and its first use cost. **Seeded synthetic textures at the real assets' sizes** (3840×1920 stars, 2048×1024
clouds) -- an RGBA8 texture's sampling cost depends on its size and access pattern, not on what the texels show, and
the section must never need the NASA downloads (the real textures measured the same: 0.34 ms vs 0.33 ms at 2x z3).
No data, no network, so `--only look` skips the census data check. Built `--features probe,orbits` with the
satellites demo. Its rows are not baselined yet: they print as ungraded until the first `--update-baseline --reps 3
--only look` on a quiet machine.

| Metric | What it is | Band |
|---|---|---|
| `look.gpu_2x_us` | GPU µs/frame the sky + globe passes add to a clear-only pass at 3456×2234 (a 16" MacBook at 2x), z3, lat 20: `space_look::measure_frame_cost`, interleaved batches of 20 submits, one wait per batch, median of 9 | time |
| `look.gpu_5k_us` | The same at 5120×2880 (a 5K display at 2x) | time |
| `look.build_ms` | `SpaceLookPipeline::new` with both full-size textures (~37 MB upload), to device idle, best of 3 after one untimed build (Metal caches the compiled shader on disk after the first) -- the first look frame's stall on the frame thread | time |
| `look.write_us` | One frame's `SpaceLookPipeline::write` (the uniform and its 128-byte write) | time |
| `look.draw_calls` | Look draws issued; the demo exits 2 on zero | info |

The GPU rows are in µs, not ms, on purpose: the grader gives every `ms` time row a ceiling of at least baseline
+ 1 ms, which on a ~0.3 ms figure would let the look quadruple unseen.

```text
scripts/perf-gate.sh headless --only look
scripts/perf-gate.sh headless --update-baseline --reps 3 --only look
```

### Windowed tier (a real, frontmost 1280×800 window)

| Mode | Metrics |
|---|---|
| `pan`: `--pan --quit-after 25`, graded from 5 s after the first overlay frame | `frame_interval_ms` (1000/fps), `gpu_pass_mean_ms`, `gpu_pass_max_ms`, CPU frame `p50`/`p99`/`max`, `frames_over_16_7ms`, holes, `rss_mb` |
| `zoom_dc`, `zoom_la`: `--zoom-sweep --at dc` or `--at la` | `overlay_holes` and `basemap_holes` (must stay 0), `frames_over_16_7ms`, CPU `p99`/`max`, GPU max, `detail_after_settle_max_ms`, `rss_at_rest_mb`, `rss_leg_max_mb` |
| `slider`: `--slider-sweep` | `drag_p50_ms` and `drag_max_ms` (table-only update latency), `drag_other_path` (must stay 0), `drag_frame_interval_ms`, `rebake_settle_{p50,max}_ms`, holes |
| `dots.pan`: `demos/world_dots.rs --synthetic --at india --pan --quit-after 25` | `dots.pan.cpu_{p95,max}_ms`, `dots.pan.frames_over_33ms` (this display's 120 Hz budget, not 16.7 ms), `dots.pan.gpu_pass_p95_ms` (the whole FRAME's GPU pass, `world_dots`' `gpu_ms_last_sample` -- not the dot mask pass: this India z5 pan has none, relocation starts at z15), `dots.pan.{rss,dot_gpu}_max_mb`. `world_dots`' own `report_frames` prints no `occluded_paints` line, so this mode (and any other `dots.`-prefixed one) is validated on draw calls alone -- see the note below. Baselined at c6ea91d (Task 11 controller ruling, 3 reps, load 2.0-4.3): `cpu_p95_ms` 33.25 ms, `cpu_max_ms` 50.50 ms, `gpu_pass_p95_ms` 30.83 ms, `frames_over_33ms` 65, `rss_max_mb` 546.50, `dot_gpu_max_mb` 9.84. **Why 33 ms is not a regression** (investigated before the merge, accepted by ruling): the synthetic India scenario is ~30× sparser than real data -- one dot per cell across the whole land box -- at ~3× the GPU cost per in-view dot, and master 067d943, c6ea91d and pre-relocation 5f3cafe all read `cpu_p95_ms` 33.2-33.6 ms on it, while a pan over the real archive reads 8.5 ms on master and the branch alike. `cpu_p95_ms` sits on the 120 Hz display's 33.3 ms vsync step (a frame that misses its slot waits for the next), so it barely moves; `frames_over_33ms` and `gpu_pass_p95_ms` are the rows that move when the dot draw gets slower. |

### Windowed tier, `live` section (`--live wave --live-secs 120`)

Incremental live updates (`MapView::update_source_features`, see
`docs/superpowers/specs/2026-09-26-incremental-overlay-updates-design.md`). At the national view, ten times a
second: 1,000 block groups nearest a meridian that sweeps the country every 20 s get a new income, and every 20th
tick 100 block groups are removed and the 100 removed the time before are added back.

| Metric | What it is | Band |
|---|---|---|
| `live.latency_{p50,p99,max}_ms` | Submit → the first frame with no hole and no tile the diff dirtied still showing its old picture (the engine's own `take_live_samples`) | time |
| `live.apply_p50_ms` | The worker's `live::apply` (columns, classes, index, hit index) | time |
| `live.update_call_max_ms` | The synchronous `update_source_features` call on the UI thread | time |
| `live.rebaked_tiles_p50`, `rebaked_fraction_p50` | Cached tiles each landed job dirtied, and the fraction of the cache | time |
| `live.rss_start_mb`, `rss_end_mb` | RSS 10 s into the live phase and at its end (drift is the difference) | bytes |
| `live.overlay_holes`, `basemap_holes`, `frames_over_16_7ms`, CPU `p50`/`p99`/`max`, GPU | As every windowed mode (`gate_frames`) | as there |

Run or re-baseline only this section with `--only live`:

```text
scripts/perf-gate.sh windowed --only live
scripts/perf-gate.sh windowed --update-baseline --reps 3 --only live
```

### Windowed tier, `sats.run` section (`demos/satellites.rs --synthetic --measure-frames 10 --rate 1000`)

The satellites layer drawing: the synthetic 10,000 plus two orbit rings (the first object's and the first GEO's), on
the keyless Positron basemap, in a frontmost 1440×900 window at lat 20, z -1, starting at the headless section's scene time with the clock at 1000×. After
2 s of warm-up it logs every `MapView::paint` for 10 s (`render::frame_log_*`) and prints
`sats.run.frame_p95_ms` -- the 95th percentile paint-to-paint interval, so GPU back-pressure is in it -- and
`sats.run.draw_calls`, the `draw_calls_total` delta over the window. It has its own prefix, as `dots.pan` does, and
like `dots.pan` prints no `occluded_paints` line, so it is validated on draw calls alone: 0 draw calls voids the run.

The A/B control is the same run with `--hide-all` (every group's chip off, no rings), not part of the gate:

```text
satellites --synthetic --measure-frames 10 --rate 1000
satellites --synthetic --measure-frames 10 --rate 1000 --hide-all
```

MG-S3 (`docs/measurements/2026-09-29-satellites.md`) passes when both runs drew (`draw_calls > 0`), the layer's
`frame_p95_ms` is at most 16.7 ms, and the layer costs at most 2.0 ms of p95 over the hidden run.

**The gate refuses to score an invalid windowed run.** A refused run is reported with its reason and is not scored:

- occluded paints > 0: the window was not frontmost (every mode except `dots.pan` and `sats.run`, which print no
  such line and are gated on draw calls alone);
- 0 draw calls: nothing painted;
- the 1-minute load average before the run was above `--max-load` (default: half the cores);
- the screen was locked before or after the run.

Every run records its load average before and after. If every run of a mode is refused, that mode scores nothing,
and the gate exits 5 (INCOMPLETE) rather than passing -- or 2 if no mode scored at all.

## buildings

3D building extrusion (`fill-extrusion`; spec
`docs/superpowers/specs/2026-09-29-3d-building-extrusion-design.md` §5.4). All three sections (`buildings`,
`buildings.pan` and `buildings.pan_opaque`) run `examples/buildings_gate.rs` and fetch no data for the headless half. Their baselines are taken by the merge's
`scripts/perf-gate.sh … --update-baseline` run.

### `buildings` (headless, `buildings_gate --measure`)

The three vendored z14 building fixtures, `benches/fixtures/{shinjuku,manhattan,singapore}_z14.pbf`. They are real
OpenFreeMap tiles: © OpenStreetMap contributors, ODbL 1.0 (provenance and fetch command in
`benches/fixtures/README.md`). Each is matched against Liberty (`examples/liberty-style.json`) at z14 with
`style::match_tile` and tessellated by `render::extrusion::tessellate_extrusions`.

| Metric | What it is | Band |
|---|---|---|
| `buildings.tess.<city>.vertices` | Extrusion vertices (roof + 4 per wall) | **exact** |
| `buildings.tess.<city>.indices` | Extrusion indices | **exact** |
| `buildings.tess.<city>.walls` | Wall quads the tile owns | **exact** (`.walls` is on the Exact list) |
| `buildings.tess.<city>.bake_ms` | Best of 3 of `match_tile` + `tessellate_extrusions`: the extrusion work a bake adds, and the match it rides on | time |
| `buildings.tess.<city>.upload_mb` | Vertex + index + table bytes of the mesh (`ExtrusionMesh::upload_bytes`) | bytes |

`<city>` is `shinjuku_z14`, `manhattan_z14` or `singapore_z14`. The counts include Liberty's
`["!=", ["get", "hide_3d"], true]` filter on `building-3d`: without it they are Task 5's numbers
(`docs/measurements/2026-09-29-building-extrusion.md`). Translucency does not change what is tessellated, so this
section is the same for opaque and translucent layers.

### `buildings.pan` and `buildings.pan_opaque` (windowed, `buildings_gate --pan [--opaque] --quit-after 25`)

Liberty on OpenFreeMap (keyless) over Shinjuku at z16, pitch 60, bearing 30, in a real, frontmost 1280×800 window,
panning about two z16 tiles each way. The graded window opens 3 s after the first frame that draws an extrusion
triangle, and closes at 25 s. A run that never draws a building prints no gate line and is refused.

- `buildings.pan` is Liberty as vendored. Its `building-3d` is opacity 0.8, so this is the **translucent** path,
  which is Liberty's default: a depth prepass, then a stencilled colour pass. The geometry is drawn twice.
- `buildings.pan_opaque` forces that layer's opacity to 1. That draws one opaque pass with no prepass. Compare the
  two sections to see what translucency costs.

| Metric | What it is | Band |
|---|---|---|
| `gpu_pass_mean_ms` | Mean of every GPU tile-pass sample in the window (`allocation_stats`) | time |
| `gpu_pass_p95_ms`, `gpu_pass_max_ms` | The whole frame's GPU pass, per frame (`zoom_probe`'s `gpu_ms_last_sample`) | time |
| `cpu_p99_ms` | CPU frame time, p99 | time |
| `frames_over_16_7ms` | CPU frames over 16.7 ms | spikes |
| `extrusion_triangles_max` | The most extrusion triangles a frame submitted, sampled every 16 ms (`map_view::extrusion_triangles_last_frame`, probe builds only). Every pass counts, so a translucent frame reports about twice its opaque twin | **count, two-sided**: max(×1.15, 1 + 2 × spread + 0.10) above, its mirror below (floor at least ×0.5), ceiling capped at the spec's 1.5M. Losing buildings fails, and so does a heavier frame |
| `rss_max_mb` | RSS at its maximum over the window | bytes |
| `occluded_paints`, `draw_calls_total`, `window_s` | Validity: a covered window or 0 draws voids the run. `window_s` is how long the graded window was | info |

**The gate is relative, not absolute.** Every row is graded against its own baseline, except
`extrusion_triangles_max`, whose ceiling is capped at 1.5M. Nothing here compares `gpu_pass_mean_ms` with §5.2's
8 ms budget. The controller checks that budget by hand, once, from the merge's baseline run.

With `--only`, the script's memory precheck uses that section's own projected peak (`perf_gate --projected-peak
SECTION`, from `MODES`). A full run is still checked against its tier's worst mode.

Run or re-baseline only these sections with `--only`:

```text
scripts/perf-gate.sh headless --only buildings
scripts/perf-gate.sh windowed --only buildings.pan
scripts/perf-gate.sh windowed --only buildings.pan_opaque
scripts/perf-gate.sh headless --update-baseline --reps 3 --only buildings
```

## Reading the output

```text
metric                                 observed     baseline    delta      allowed  result
headless.cover.dc_z8.outline_indices    1398812       703102   +98.9%     =703102  FAIL count
headless.push_ms                         270.51       271.02    -0.2%     <=317.10  PASS ms
```

- **observed** is the p50 of the valid runs.
- **allowed** is one of three forms:
  - `<=X`: a ceiling, `baseline × tolerance + slack`.
  - `=X`: exact.
  - `A-B`: a two-sided band.
- **A FAIL prints the band's rationale**: the runs the band came from, their spread, and the rule applied.
- **SLOW (advisory)** means a headless failure on a busy machine, by the stress suite's `Env::quiet`. It does not
  fail the gate. Rerun on a quiet machine.
- **MISSING** means a baselined metric that no valid run reported. It usually means the demo stopped printing it,
  so treat it as a regression in the gate itself. Any MISSING row makes the gate **INCOMPLETE** (exit 5), not a
  pass: 5239ac7's `headless_states.table_only.*` went MISSING in a gate that printed PASS, because a sibling's
  non-probe build had re-pointed the shared `release/examples/us_block_groups` link mid-run.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Pass |
| 1 | Regression |
| 2 | Not scored: data missing, no valid run, or no baseline |
| 3 | Refused by a safety check: free plus inactive memory is below the projected peak plus 8 GB, or the disk has under 3 GB free |
| 4 | Build failure |
| 5 | Incomplete: every graded row passed, but a baselined row was MISSING or a mode had no valid run |

A FAIL wins over everything else (exit 1 even when rows are also missing); NOT SCORED (2) wins over INCOMPLETE (5).
Verdict line and code come from `gate_outcome` in `examples/perf_gate.rs`, which has a test per outcome.

Two guards keep a run from grading a binary it did not build. `scripts/perf-gate.sh` runs a private copy of the
four binaries it built, not the shared `release/examples/` links; and `us_block_groups`, `big_points` and
`world_dots` refuse to run with `US_BG_GATE` set unless built with `--features probe` (`GATE ERROR`, exit 2, which
the gate reports as an invalid run).

Logs from each run are kept in `$TMPDIR/perf-gate/<mode>-<n>.log`.

## Bands

Bands follow the format and grader of `stress/baseline.json` (`examples/common/stress.rs`). Two optional fields
were added, and older baselines ignore them:

- `floor`: a count that falls is also a change.
- `slack`: an absolute allowance, for a baseline of 0.

`--update-baseline` sets each band from the measured spread. The spread is (max − min) / p50 over at least 3 runs.
Each band is then set by one of these rules:

| Kind | Rule |
|---|---|
| Time | max(×1.15, 1 + 2 × spread + 0.10) |
| Bytes | max(×1.05, 1 + 2 × spread + 0.02) |
| Deterministic counts (vertices, indices, walls, wanted tiles, hits) | Exact, both sides. If the runs disagree, ±5%, and the rationale says to investigate. |
| Holes, off-path updates, unbaked tiles, nesting violations | May not rise above the baseline, which is 0. |
| Near-deterministic counts that must not fall or rise (`extrusion_triangles_max`) | max(×1.15, 1 + 2 × spread + 0.10) above, its mirror below (floor at least ×0.5), and the ceiling never above a cap (1.5M, spec §5.2) |
| Frames over 16.7 ms (or, at 120 Hz, over 33 ms) | Baseline plus a slack of max(3, 2 × the worst run). |
| A timing with a spec budget (`story.card.frame_ms_p50` 4.0 ms, `story.card.layout_us_p50` 300 µs; ruling P16) | The Time band, then cut so the ceiling never passes the budget: the slack first, then the tolerance (rounded down). A baseline already over the budget gets a tolerance under 1, so the next run fails. |
| A fixed absolute cap (`dots.world_total_err_pct`) | The ceiling is the cap itself (0.1), never the baseline scaled by a multiplier -- the recorded baseline is forced to 0 so a later, larger measured value cannot widen the tolerance it is graded against. |

Fields that only show whether a run is valid (`occluded_paints`, `draw_calls`, `draw_calls_total`, `frames`, `fps`,
`updates`, `window_s`) are printed as `info` and never graded.

## Re-baselining

Re-baseline only when a change is **meant** to move a number, for example a new LOD rule that changes vertex counts.
Do it on a quiet machine:

```text
scripts/perf-gate.sh headless --update-baseline --reps 5
scripts/perf-gate.sh windowed --update-baseline --reps 3
git diff stress/us_block_groups_baseline.json
```

The run prints every band as `old -> new` and writes `stress/us_block_groups_baseline.json`. Commit the file in its
own commit, and say in the message why the number moved. A band that someone widened silently has stopped being a
gate. Updating one tier keeps the other tier's entries.

## Self-test

`GPUI_MAP_PROBE_OUTLINE_LOD=off` works in probe builds only. It bakes every outline, ignoring the level of detail in
`src/overlay/tile_plan.rs`. That is a realistic regression: more outline indices, slower bakes and a heavier GPU pass.
With it set, the gate must FAIL. Without it, the gate must PASS:

```text
GPUI_MAP_PROBE_OUTLINE_LOD=off scripts/perf-gate.sh headless   # exits 1
scripts/perf-gate.sh headless                                  # exits 0
```

## What it does not cover

- **Network variance.** The windowed tier fetches basemap tiles from OpenFreeMap. A slow or failed fetch shows up
  as basemap stand-ins or holes. That is why basemap holes are graded, and why a red basemap-hole result should be
  rerun before anyone believes it.
- **Other machines.** Every band is specific to this machine: Apple silicon, 48 GB, 14 cores. On any other machine,
  re-baseline first.
- **Pacing.** The sweeps drive the camera on a 16 ms timer, so `frame_interval_ms` can never go below about
  16–20 ms. It catches a slowdown, not a speedup.

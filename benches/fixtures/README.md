# Benchmark and test fixtures

All three are real OpenFreeMap vector tiles (OpenMapTiles schema). The data is
© OpenStreetMap contributors and available under the Open Database License
(ODbL) 1.0, https://opendatacommons.org/licenses/odbl/. Served by OpenFreeMap,
https://openfreemap.org.

| file | tile | OpenFreeMap build | building features | used by |
|---|---|---|---|---|
| `singapore_z14.pbf` | 14/12918/8134 | (pre-existing; see `benches/perf.rs`) | 74 | perf bench, road shields, compact, columnar |
| `shinjuku_z14.pbf` | 14/14549/6451 | 20260913_164504_pt | 268 | extrusion tessellation, `buildings` perf gate |
| `manhattan_z14.pbf` | 14/4824/6157 | 20260913_164504_pt | 1488 | extrusion tessellation, `buildings` perf gate |

Fetched with `curl -A gpui-map-fixture/0.1 https://tiles.openfreemap.org/planet/<build>/14/<x>/<y>.pbf`,
un-gzipped if needed.

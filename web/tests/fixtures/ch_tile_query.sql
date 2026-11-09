-- The SQL `web/serve.py`'s /clickhouse/ proxy must build, case by case.
-- Written by the Rust test `web_proxy_template_fixture` (src/overlay/dotgrid/clickhouse.rs);
-- read by serve.py's `test_proxy_template_matches_rust`. Do not edit by hand:
-- GPUI_MAP_WRITE_FIXTURES=1 cargo test web_proxy_template_fixture
-- case tile table=ghs_cells_30ss epoch=2025 level=0 tiles=2883611,2949147,3014683,3080219,3145755,3211291,3276827,3342363
SELECT toUInt32(row * transform(tile, [2883611, 2949147, 3014683, 3080219, 3145755, 3211291, 3276827, 3342363], [675, 675, 675, 675, 675, 675, 675, 675], 0) + col + transform(tile, [2883611, 2949147, 3014683, 3080219, 3145755, 3211291, 3276827, 3342363], [-5127975, 11648566, 28425107, 45201648, 61978189, 78754730, 95531271, 112307812], 0)) AS k,
       toUInt32(floor(pop * 10 + 0.5)) AS deci
FROM gpui_map_dots.ghs_cells_30ss
WHERE epoch = 2025 AND level = 0 AND tile IN (2883611, 2949147, 3014683, 3080219, 3145755, 3211291, 3276827, 3342363) AND deci > 0
ORDER BY tile, row, col
FORMAT RowBinary
SETTINGS use_query_cache = 1, enable_http_compression = 0
-- case tile table=ghs_cells_30ss epoch=1975 level=3 tiles=0,458759,196612
SELECT toUInt32(row * transform(tile, [0, 196612, 458759], [675, 675, 675], 0) + col + transform(tile, [0, 196612, 458759], [-49950, 15863941, 31836557], 0)) AS k,
       toUInt32(floor(pop * 10 + 0.5)) AS deci
FROM gpui_map_dots.ghs_cells_30ss
WHERE epoch = 1975 AND level = 3 AND tile IN (0, 196612, 458759) AND deci > 0
ORDER BY tile, row, col
FORMAT RowBinary
SETTINGS use_query_cache = 1, enable_http_compression = 0
-- case tile table=ghs_cells_30ss epoch=2030 level=6 tiles=0
SELECT toUInt32(row * transform(tile, [0], [675], 0) + col + transform(tile, [0], [-6075], 0)) AS k,
       toUInt32(floor(pop * 10 + 0.5)) AS deci
FROM gpui_map_dots.ghs_cells_30ss
WHERE epoch = 2030 AND level = 6 AND tile IN (0) AND deci > 0
ORDER BY tile, row, col
FORMAT RowBinary
SETTINGS use_query_cache = 1, enable_http_compression = 0
-- case tile table=ghs_cells_30ss epoch=2020 level=1 tiles=2031630,16,2031647
SELECT toUInt32(row * transform(tile, [16, 2031630, 2031647], [675, 675, 675], 0) + col + transform(tile, [16, 2031630, 2031647], [-3645000, 14000266, 26487182], 0)) AS k,
       toUInt32(floor(pop * 10 + 0.5)) AS deci
FROM gpui_map_dots.ghs_cells_30ss
WHERE epoch = 2020 AND level = 1 AND tile IN (16, 2031630, 2031647) AND deci > 0
ORDER BY tile, row, col
FORMAT RowBinary
SETTINGS use_query_cache = 1, enable_http_compression = 0
-- case tile table=ghs_cells_3ss epoch=2025 level=0 tiles=25231582,25297118
SELECT toUInt32(row * transform(tile, [25231582, 25297118], [843, 844], 0) + col + transform(tile, [25231582, 25297118], [-67857574, -51161311], 0)) AS k,
       toUInt32(floor(pop * 10 + 0.5)) AS deci
FROM gpui_map_dots.ghs_cells_3ss
WHERE epoch = 2025 AND level = 0 AND tile IN (25231582, 25297118) AND deci > 0
ORDER BY tile, row, col
FORMAT RowBinary
SETTINGS use_query_cache = 1, enable_http_compression = 0
-- case tile table=ghs_cells_3ss epoch=2025 level=4 tiles=1572877
SELECT toUInt32(row * transform(tile, [1572877], [844], 0) + col + transform(tile, [1572877], [-3695026], 0)) AS k,
       toUInt32(floor(pop * 10 + 0.5)) AS deci
FROM gpui_map_dots.ghs_cells_3ss
WHERE epoch = 2025 AND level = 4 AND tile IN (1572877) AND deci > 0
ORDER BY tile, row, col
FORMAT RowBinary
SETTINGS use_query_cache = 1, enable_http_compression = 0
-- case epochs table=ghs_cells_30ss
SELECT DISTINCT epoch FROM gpui_map_dots.ghs_cells_30ss ORDER BY epoch FORMAT TabSeparated
-- case max_level table=ghs_cells_30ss
SELECT max(level) FROM gpui_map_dots.ghs_cells_30ss FORMAT TabSeparated
-- case epochs table=ghs_cells_3ss
SELECT DISTINCT epoch FROM gpui_map_dots.ghs_cells_3ss ORDER BY epoch FORMAT TabSeparated
-- case max_level table=ghs_cells_3ss
SELECT max(level) FROM gpui_map_dots.ghs_cells_3ss FORMAT TabSeparated

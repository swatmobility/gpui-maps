#!/usr/bin/env bash
# Loads GHS-POP R2023A 30″ epochs into gpui_map_dots.ghs_cells_30ss, one at a
# time (Task 15, live epochs): for each epoch not in the table yet, check the
# disk, download it (`scripts/fetch-ghsl.sh EPOCH 30ss`), load it
# (`dotgrid-build --load-clickhouse`, which runs the H3 gate MG-13 and the
# per-level sum checks, and swaps the epoch in only when they pass), then
# delete the TIFF -- only one this run downloaded; a TIFF already on disk is
# loaded and kept. Only one epoch's download is ever on disk.
#
#     scripts/load-ghsl-epochs.sh                 # every epoch 1975..2030 not loaded
#     scripts/load-ghsl-epochs.sh 1975 2030       # just these
#
# Disk rule (Task 15 brief): free >= 1 GB peak + the epoch's table growth
# (MG-12 bytes/row x the largest loaded epoch's rows, from system.parts;
# an empty table is refused, exit 6) + 10 GB, checked before every download. Short: stop, exit 3, and say
# which epochs are in. Binary: $DOTGRID_BUILD, default the release build
# under $CARGO_TARGET_DIR. One line per epoch in $DATA/epochs-30ss.tsv.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DATA=${GPUI_MAP_DATA:-$HOME/.cache/gpui-map-data}/ghsl
BIN=${DOTGRID_BUILD:-${CARGO_TARGET_DIR:-$ROOT/tools/dotgrid-build/target}/release/dotgrid-build}
[[ -x "$BIN" ]] || { echo "no dotgrid-build at $BIN" >&2; exit 2; }
CH=${CLICKHOUSE_URL:-http://localhost:8123}
TABLE=gpui_map_dots.ghs_cells_30ss
TSV=$DATA/epochs-30ss.tsv
mkdir -p "$DATA"
[[ -s "$TSV" ]] || printf 'epoch\tfree_mb_before\tneed_mb\tfetch_s\tload_s\tbytes_on_disk_after\trows_after\n' > "$TSV"

q() { curl -fsS --max-time 60 "$CH/" --data-binary "$1"; }

if (( $# )); then epochs=("$@"); else epochs=(1975 1980 1985 1990 1995 2000 2005 2010 2015 2020 2025 2030); fi

loaded() { q "SELECT DISTINCT epoch FROM $TABLE ORDER BY epoch FORMAT TabSeparated" | tr '\n' ' '; }

# MG-12 x rows: the table's bytes per row now, times the LARGEST loaded
# epoch's rows (every level) -- in MB, rounded up. An empty table has
# neither, and a new epoch's size cannot be guessed from nothing: refuse
# (load the first epoch with `dotgrid-build --load-clickhouse`, whose own
# check uses the brief's 1 GB estimate). Review M-4.
sizes=$(q "SELECT sum(rows), sum(bytes_on_disk) FROM system.parts WHERE database = 'gpui_map_dots' AND table = 'ghs_cells_30ss' AND active FORMAT TabSeparated")
read -r rows bytes <<< "$sizes"
largest=$(q "SELECT max(c) FROM (SELECT count() AS c FROM $TABLE GROUP BY epoch) FORMAT TabSeparated")
if [[ -z "$rows" || "$rows" == 0 || -z "$largest" || "$largest" == 0 ]]; then
    echo "refusing: $TABLE is empty, so no epoch's size is known to size the disk rule by" >&2
    exit 6
fi
# ceil(bytes / rows * largest / 2^20), in integers: no division by zero past the check above.
growth_mb=$(( (bytes * largest / rows + 1048575) / 1048576 ))
need_mb=$(( 1024 + growth_mb ))
echo "table growth per epoch ${growth_mb} MB; need ${need_mb} MB + 10240 MB floor per download" >&2

for e in "${epochs[@]}"; do
    if [[ " $(loaded) " == *" $e "* ]]; then
        echo "E$e: already loaded" >&2
        continue
    fi
    free=$(df -m "$DATA" | awk 'NR==2{print $4}')
    if (( free < need_mb + 10240 )); then
        echo "E$e: STOP -- ${free} MB free < ${need_mb} + 10240 MB. Loaded: $(loaded)" >&2
        exit 3
    fi
    t0=$SECONDS
    # A TIFF already on disk (someone else's download) is loaded, never
    # deleted: only one this run's fetch created is removed (review M-3).
    # fetch-ghsl.sh names it ${DATA}/<zip name>.tif; the pattern matches
    # that name for this epoch.
    pre_existing=$(find "$DATA" -maxdepth 1 -name "GHS_POP_E${e}_GLOBE_R2023A_4326_30ss_*.tif" -print)
    tif=$(GHSL_NEED_MB=$need_mb "$ROOT/scripts/fetch-ghsl.sh" "$e" 30ss | tail -1)
    t1=$SECONDS
    [[ -s "$tif" ]] || { echo "E$e: fetch gave no TIFF" >&2; exit 4; }
    set +e
    "$BIN" --load-clickhouse --tiff "$tif" --arcsec 30 --epoch "$e" --report "$DATA/E${e}_30ss.clickhouse.json" >&2
    rc=$?
    set -e
    t2=$SECONDS
    # Delete the TIFF this run downloaded, loaded or not -- and only that.
    if [[ -n "$pre_existing" ]] && grep -qxF "$tif" <<< "$pre_existing"; then
        echo "E$e: $tif was on disk before this run; kept" >&2
    else
        rm -f "$tif"
    fi
    if (( rc != 0 )); then
        echo "E$e: load failed (exit $rc); the epoch was not swapped in. Loaded: $(loaded)" >&2
        exit 5
    fi
    stats=$(q "SELECT sum(bytes_on_disk), sum(rows) FROM system.parts WHERE database = 'gpui_map_dots' AND table = 'ghs_cells_30ss' AND active FORMAT TabSeparated")
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$e" "$free" "$need_mb" "$((t1 - t0))" "$((t2 - t1))" "$stats" >> "$TSV"
    echo "E$e: loaded (fetch $((t1 - t0)) s, load $((t2 - t1)) s); $stats" >&2
done
echo "done. Loaded: $(loaded)" >&2

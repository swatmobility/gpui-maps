#!/usr/bin/env bash
# Downloads one GHSL GHS-POP R2023A epoch in EPSG:4326 from the JRC open-data
# server and unzips its GeoTIFF(s) for `tools/dotgrid-build`:
#
#     scripts/fetch-ghsl.sh 2025 30ss    # the single global 30″ zip (≈ 461 MB)
#     scripts/fetch-ghsl.sh 2025 3ss     # the 348 10°×10° 3″ tiles (≈ 9.3 GB)
#
# Everything lands under ${GPUI_MAP_DATA:-~/.cache/gpui-map-data}/ghsl -- the
# internal disk, never the repo. No GDAL anywhere (it is broken on this
# machine); the converter reads the TIFFs with the pure-Rust `tiff` crate.
#
# 30ss: prints the path of the unzipped TIFF on stdout (last line).
#
# 3ss: drives the converter. It starts `dotgrid-build --tiff-list
# $DATA/E${EPOCH}_3ss.list --delete-consumed` (binary: $DOTGRID_BUILD, default
# the release build under ${CARGO_TARGET_DIR:-tools/dotgrid-build/target}) and
# appends each unzipped TIFF's path to that list as it arrives, then an `END`
# line; the converter bins each TIFF as its path appears and deletes it, so
# only a couple of tiles are ever on disk (the script waits while two are
# queued). Output: $DATA/E${EPOCH}_3ss.pmtiles and .report.json; the
# converter's stderr (with `/usr/bin/time -l`) in .build.log; one line per
# zip (bytes, seconds, MB/s) in .fetch.tsv.
#
# Resumable: every zip whose TIFF reached the list is named in
# $DATA/E${EPOCH}_3ss.done. Rerun the same command after an interruption and
# it skips those, continues a partial download (`curl -C -`), and starts the
# converter with `--resume` (it checkpoints after every TIFF). Delete the
# .done file to start over. This script never runs more than one download at
# a time (be polite to the JRC server; check every size).
#
# Exit codes: 2 usage, 3 not enough disk, 4 a download never reached its
# listed size, 5 the listing could not be read, 6 the converter failed.
set -euo pipefail

EPOCH=${1:-}
RES=${2:-}
if [[ -z "$EPOCH" || ( "$RES" != "3ss" && "$RES" != "30ss" ) ]]; then
    echo "usage: $0 EPOCH 3ss|30ss" >&2
    exit 2
fi

DATA=${GPUI_MAP_DATA:-$HOME/.cache/gpui-map-data}/ghsl
mkdir -p "$DATA"

B=https://jeodpp.jrc.ec.europa.eu/ftp/jrc-opendata/GHSL/GHS_POP_GLOBE_R2023A
PRODUCT=GHS_POP_E${EPOCH}_GLOBE_R2023A_4326_${RES}
if [[ "$RES" == "3ss" ]]; then
    LISTING_URL=$B/$PRODUCT/V1-0/tiles/
else
    LISTING_URL=$B/$PRODUCT/V1-0/
fi

# The listing's own size column is human-rounded ("461M"), so every zip's
# exact size comes from a HEAD request's Content-Length instead.
listing=$(curl -fsS --max-time 60 "$LISTING_URL") || { echo "cannot read $LISTING_URL" >&2; exit 5; }
zips=$(printf '%s\n' "$listing" | grep -o 'href="[^"]*\.zip"' | sed 's/^href="//; s/"$//' | sort -u)
if [[ -z "$zips" ]]; then
    echo "no .zip in $LISTING_URL" >&2
    exit 5
fi

head_size() {
    curl -fsSI --max-time 60 "$1" | tr -d '\r' | awk 'tolower($1)=="content-length:"{print $2}' | tail -1
}

declare -a names=() sizes=()
total=0
largest=0
for z in $zips; do
    s=$(head_size "$LISTING_URL$z")
    [[ -n "$s" ]] || { echo "no Content-Length for $z" >&2; exit 5; }
    names+=("$z")
    sizes+=("$s")
    total=$((total + s))
    (( s > largest )) && largest=$s
done
echo "listing: ${#names[@]} zip(s), $((total / 1048576)) MB in total, largest $((largest / 1048576)) MB" >&2

# Disk guard: two of the largest zip in flight plus the expected output
# (30ss: the unzipped world TIFF and the archive; 3ss: unzipped tiles, the
# spill and the archive), plus a 10 GB floor that is never spent.
# 3ss output, from MG-17 (E2025): the level-0 spill is 8 B per populated
# cell x 1.41 B cells = 10.8 GiB at the end of the bin pass. The pyramid
# keeps a level's spill until the level is done (so a kill resumes at a
# level boundary), so level 0 peaks with level 1's spill (0.46 B cells,
# 3.4 GiB) and level 0's encoded tiles (1.1 GB) beside it: ≈ 15.3 GiB. The
# archive write then needs the encoded tiles and the archive (1.9 GB each)
# only. 16,000 MB covers the peak.
# A resumed run's spill is already on disk, so it is not counted twice.
if [[ "$RES" == "30ss" ]]; then output_mb=3000; else output_mb=16000; fi
need_mb=$(( 2 * largest / 1048576 + output_mb ))
# A caller that sized its own need (`scripts/load-ghsl-epochs.sh`: the
# brief's 1 GB peak + the epoch's table growth, the TIFF only -- no
# converter archive) says so; the 10 GB floor is still added below.
if [[ -n "${GHSL_NEED_MB:-}" && "$RES" == "30ss" ]]; then need_mb=$GHSL_NEED_MB; fi
check_disk() {
    local avail
    avail=$(df -m "$DATA" | awk 'NR==2{print $4}')
    echo "disk: ${avail} MB free on $(df -m "$DATA" | awk 'NR==2{print $1}'), need $1 MB + 10240 MB floor" >&2
    if (( avail < $1 + 10240 )); then
        echo "refusing: ${avail} MB free < $1 + 10240 MB" >&2
        exit 3
    fi
}

LIST=$DATA/E${EPOCH}_${RES}.list
DONE=$DATA/E${EPOCH}_${RES}.done
resume=0
if [[ "$RES" == "3ss" && -s "$DONE" ]]; then
    resume=1
    echo "resuming: $(wc -l < "$DONE" | tr -d ' ') of ${#names[@]} zips already listed" >&2
    # The spill for those is on disk already; only the rest is still to
    # come: the spill measured 1.19 bytes per zip byte (2025, 3ss), so twice
    # the zips left. The pyramid and the archive write then only shrink the
    # footprint (encoded tiles measured 1.17 B per cell against the spill's
    # 8) -- except level 0, whose level-1 spill and encoded tiles (≈ 4.5 GB)
    # sit beside the level-0 spill until it is done -- and the converter
    # checks the disk again before the archive write.
    left=0
    left_largest=0
    for i in "${!names[@]}"; do
        grep -qxF "${names[$i]}" "$DONE" && continue
        left=$((left + sizes[i]))
        (( sizes[i] > left_largest )) && left_largest=${sizes[i]}
    done
    need_mb=$(( (2 * left_largest + 2 * left) / 1048576 + 4600 ))
fi
check_disk "$need_mb"

if [[ "$RES" == "3ss" ]]; then
    ROOT=$(cd "$(dirname "$0")/.." && pwd)
    BIN=${DOTGRID_BUILD:-${CARGO_TARGET_DIR:-$ROOT/tools/dotgrid-build/target}/release/dotgrid-build}
    [[ -x "$BIN" ]] || { echo "no converter at $BIN (build tools/dotgrid-build --release, or set DOTGRID_BUILD)" >&2; exit 2; }
    OUT=$DATA/E${EPOCH}_3ss.pmtiles
    TSV=$DATA/E${EPOCH}_3ss.fetch.tsv
    if (( ! resume )); then
        : > "$LIST"
        : > "$DONE"
        printf 'zip\tbytes\tseconds\tMB_per_s\n' > "$TSV"
    fi
    SPILL=${GPUI_MAP_DATA:-$HOME/.cache/gpui-map-data}/spill
    flags=(--tiff-list "$LIST" --delete-consumed --arcsec 3 --epoch "$EPOCH" --out "$OUT" --report "${OUT%.pmtiles}.report.json" --spill-dir "$SPILL")
    # The converter refuses --resume without a checkpoint. None yet means it
    # never finished a TIFF, so every listed TIFF is still on disk: start it
    # fresh over the same list.
    if (( resume )) && [[ -f "$SPILL/E${EPOCH}_3ss/checkpoint.json" ]]; then
        flags+=(--resume)
    fi
    /usr/bin/time -l "$BIN" "${flags[@]}" > "${OUT%.pmtiles}.build.stdout" 2>> "${OUT%.pmtiles}.build.log" &
    CONV=$!
    echo "converter: pid $CONV, log ${OUT%.pmtiles}.build.log" >&2
    stop() {
        # `time` does not pass signals on: stop the converter itself too.
        pkill -TERM -P "$CONV" 2>/dev/null || true
        kill "$CONV" 2>/dev/null || true
        echo "interrupted: rerun the same command to resume" >&2
        exit 130
    }
    trap stop INT TERM
    # Waits while `$1` or more listed TIFFs are still on disk (the converter
    # has not reached them), for at most 30 minutes; fails if it died.
    wait_for_converter() {
        local deadline=$((SECONDS + 1800)) queued
        while :; do
            kill -0 "$CONV" 2>/dev/null || { echo "the converter exited early; see its log" >&2; exit 6; }
            queued=$(find "$DATA" -maxdepth 1 -name "${PRODUCT}_*.tif" | wc -l | tr -d ' ')
            (( queued < $1 )) && return 0
            (( SECONDS > deadline )) && { echo "the converter made no progress in 30 min" >&2; stop; }
            sleep 1
        done
    }
fi

fetch_one() {
    local name=$1 size=$2 part=$DATA/$1.part attempt got stats
    for attempt in 1 2 3; do
        # -C - resumes a partial file from an earlier run or attempt.
        stats=$(curl -fsSL --retry 3 --retry-delay 5 -C - -o "$part" \
            -w '%{size_download} %{time_total}' "$LISTING_URL$name") || true
        got=$(stat -f%z "$part" 2>/dev/null || echo 0)
        read -r bytes secs <<< "${stats:-0 0}"
        mbps=$(awk -v b="$bytes" -v s="$secs" 'BEGIN{printf "%.2f", (s > 0 ? b / s / 1048576 : 0)}')
        echo "$name: $bytes bytes in $secs s ($mbps MB/s), $got of $size on disk" >&2
        [[ -n "${TSV:-}" ]] && printf '%s\t%s\t%s\t%s\n' "$name" "$bytes" "$secs" "$mbps" >> "$TSV"
        if [[ "$got" == "$size" ]]; then
            mv "$part" "$DATA/$name"
            return 0
        fi
        echo "$name: $got of $size bytes after attempt $attempt" >&2
        # A file LONGER than listed cannot be resumed into shape.
        (( got > size )) && rm -f "$part"
    done
    echo "$name never reached its listed size $size" >&2
    exit 4
}

if [[ "$RES" == "30ss" ]]; then
    for i in "${!names[@]}"; do
        name=${names[$i]}
        tif=$DATA/${name%.zip}.tif
        if [[ -s "$tif" ]]; then
            echo "$tif already unzipped; skipping download" >&2
        else
            fetch_one "$name" "${sizes[$i]}"
            # Only the TIFF: the zips also carry a PDF and a copyright note.
            unzip -o -j "$DATA/$name" '*.tif' -d "$DATA" >&2
            rm -f "$DATA/$name"
            [[ -s "$tif" ]] || { echo "$name held no ${tif##*/}" >&2; exit 5; }
        fi
        echo "$tif"
    done
    exit 0
fi

started=$SECONDS
n=0
for i in "${!names[@]}"; do
    name=${names[$i]}
    grep -qxF "$name" "$DONE" && continue
    tif=$DATA/${name%.zip}.tif
    wait_for_converter 2
    # The spill grows as tiles are binned: keep the floor free throughout.
    if ! ( check_disk $(( 2 * largest / 1048576 )) ) 2> /dev/null; then
        df -m "$DATA" >&2
        echo "refusing to download $name: under 2 x the largest zip + 10240 MB free" >&2
        pkill -TERM -P "$CONV" 2>/dev/null || true
        kill "$CONV" 2>/dev/null || true
        exit 3
    fi
    [[ -s "$DATA/$name" ]] || fetch_one "$name" "${sizes[$i]}"
    # Only the TIFF (the zips also carry a PDF and a spreadsheet), into a
    # scratch dir first so the converter never sees a half-written file.
    rm -rf "$DATA/.unzip"
    unzip -q -o -j "$DATA/$name" '*.tif' -d "$DATA/.unzip" >&2
    [[ -s "$DATA/.unzip/${tif##*/}" ]] || { echo "$name held no ${tif##*/}" >&2; stop; }
    mv "$DATA/.unzip/${tif##*/}" "$tif"
    echo "$tif" >> "$LIST"
    echo "$name" >> "$DONE"
    rm -f "$DATA/$name"
    n=$((n + 1))
    echo "[$(wc -l < "$DONE" | tr -d ' ')/${#names[@]}] $name listed" >&2
done
rm -rf "$DATA/.unzip"
echo END >> "$LIST"
echo "downloads done: $n zip(s) this run in $((SECONDS - started)) s; waiting for the converter" >&2
trap - INT TERM
if ! wait "$CONV"; then
    echo "the converter failed; see ${OUT%.pmtiles}.build.log" >&2
    exit 6
fi
echo "$OUT"
exit 0

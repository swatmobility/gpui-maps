#!/usr/bin/env bash
# Downloads the 2020 Census bulk files `tools/race-build` reads (Task 1 of
# docs/superpowers/plans/2026-09-27-us-race-dot-map.md). No API key needed --
# these are static files under www2.census.gov (spec §2.3).
#
#     scripts/fetch-census-2020.sh --pl                # 51 P.L. 94-171 zips, ~1.33 GB
#     scripts/fetch-census-2020.sh --dhc-dc             # DC's DHC zip, for MG-R1
#     scripts/fetch-census-2020.sh --tiger 06           # one state's TIGER/Line block zip
#     scripts/fetch-census-2020.sh --tiger-stream [build flags]
#                                                       # all 51 states, streamed through the converter
#     scripts/fetch-census-2020.sh --pl --dhc-dc
#     scripts/fetch-census-2020.sh --cb-states          # 1:20M state outlines, ~190 kB, for
#                                                       # scripts/gen-race-states.py
#
# Everything lands under ${GPUI_MAP_DATA:-~/.cache/gpui-map-data}/census2020
# -- the internal disk, never the repo (Global Constraints: "Data never
# enters git"):
#
#     pl/<abbr>2020.pl.zip
#     dhc/dc2020.dhc.zip
#     tiger/tl_2020_<FIPS>_tabblock20.zip
#     cb/cb_2020_us_state_20m.zip
#
# Disk (Global Constraints / spec §2.5): before any download, checks
# `df -m` on the target volume and refuses, printing the numbers, when free
# space is below the step's need plus 10 GB (PL: 1.4 GB; DHC (DC only): well
# under 1 GB; TIGER in flight: 2.5 GB). This script only ever downloads and
# keeps zips -- spec §2.5's `--keep-zips` / 25 GB threshold is Task 5's
# TIGER-to-PMTiles *converter*'s own flag (it deletes each TIGER zip after
# conversion unless told to keep it); this script never deletes anything it
# fetches, so that flag has no meaning here and isn't one of its arguments.
#
# Resumable: every download goes to `<name>.part`, then is `mv`-ed into
# place only once its size matches a `curl -sIL` HEAD's Content-Length; a
# file already at that exact size is skipped. A size mismatch deletes the
# partial file and the whole run fails (exit 4), so a truncated download is
# never silently kept.
#
# `--tiger-stream` (Task 5) hands over to `race-build build`, which drives
# the national conversion one state at a time: it calls back into this
# script's `--tiger FIPS` for each download (at most 2 in flight, the next
# state fetched while the current one converts), checks `df` before every
# state (zip + unpacked + 10 GB), and deletes each zip once its state's
# checks and inside-check pass. Extra arguments go to `build` unchanged.
# The converter passes GPUI_MAP_TIGER_NEED_MB (its zip + unpacked estimate)
# so the `--tiger` disk check below is the ruling's, not the flat 2.5 GB.
#
# Exit codes: 2 usage, 3 not enough disk, 4 a download's size didn't match
# its HEAD, 5 a HEAD request came back with no Content-Length (PL, DHC or
# TIGER alike -- there is no runtime index fetch to fail separately: state
# directory names are the static STATE_DIRS list below, checked by hand
# against the Bureau's live listing when this script was written, not
# scraped at request time).
set -euo pipefail

DATA="${GPUI_MAP_DATA:-$HOME/.cache/gpui-map-data}/census2020"
PL_BASE="https://www2.census.gov/programs-surveys/decennial/2020/data/01-Redistricting_File--PL_94-171"
DHC_BASE="https://www2.census.gov/programs-surveys/decennial/2020/data/demographic-and-housing-characteristics-file"
TIGER_BASE="https://www2.census.gov/geo/tiger/TIGER2020/TABBLOCK20"
CB_BASE="https://www2.census.gov/geo/tiger/GENZ2020/shp"

DO_PL=0
DO_DHC_DC=0
DO_CB_STATES=0
TIGER_FIPS=""

if [[ "${1:-}" == "--tiger-stream" ]]; then
    shift
    ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    exec cargo run --release --manifest-path "$ROOT/tools/race-build/Cargo.toml" -- build "$@"
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --pl) DO_PL=1; shift ;;
        --dhc-dc) DO_DHC_DC=1; shift ;;
        --cb-states) DO_CB_STATES=1; shift ;;
        --tiger) TIGER_FIPS="${2:-}"; shift 2 ;;
        *) echo "usage: $0 [--pl] [--dhc-dc] [--cb-states] [--tiger FIPS] | --tiger-stream [build flags]" >&2; exit 2 ;;
    esac
done

if [[ $DO_PL -eq 0 && $DO_DHC_DC -eq 0 && $DO_CB_STATES -eq 0 && -z "$TIGER_FIPS" ]]; then
    echo "usage: $0 [--pl] [--dhc-dc] [--cb-states] [--tiger FIPS]" >&2
    exit 2
fi

mkdir -p "$DATA/pl" "$DATA/dhc" "$DATA/tiger" "$DATA/cb"

# `df -m` on the volume $DATA lives on; refuses (exit 3) below need+10 GB.
# Takes the step's own need in MB (not GB) so it can express e.g. the PL
# step's 1.4 GB exactly -- bash's `(( ))` is integer-only, so a GB argument
# here would round down and silently understate the real need.
need_free_mb() {
    local need_step_mb="$1"
    local avail_mb
    avail_mb=$(df -m "$DATA" | tail -1 | awk '{print $4}')
    local need_mb=$(( need_step_mb + 10 * 1024 ))
    if (( avail_mb < need_mb )); then
        echo "only ${avail_mb} MB free on $(df -m "$DATA" | tail -1 | awk '{print $NF}'), need ${need_mb} MB (step needs ${need_step_mb} MB + 10 GB)" >&2
        exit 3
    fi
    echo "df: ${avail_mb} MB free, need ${need_mb} MB" >&2
}

# Downloads $url to $dest, resumable and size-checked against a HEAD.
# Skips the download entirely when $dest already exists at the HEAD's size.
fetch_checked() {
    local url="$1" dest="$2"
    local want_size
    want_size=$(curl -sIL "$url" | tr -d '\r' | awk 'tolower($1)=="content-length:"{n=$2} END{print n}')
    if [[ -z "$want_size" ]]; then
        echo "no Content-Length from HEAD $url" >&2
        exit 5
    fi
    if [[ -f "$dest" ]]; then
        local got_size
        got_size=$(stat -f%z "$dest" 2>/dev/null || stat -c%s "$dest")
        if [[ "$got_size" == "$want_size" ]]; then
            echo "skip (already at ${want_size}B): $dest" >&2
            return 0
        fi
    fi
    curl -fsSL --retry 3 -o "${dest}.part" "$url"
    local got_size
    got_size=$(stat -f%z "${dest}.part" 2>/dev/null || stat -c%s "${dest}.part")
    if [[ "$got_size" != "$want_size" ]]; then
        rm -f "${dest}.part"
        echo "size mismatch for $url: got ${got_size}B, HEAD said ${want_size}B" >&2
        exit 4
    fi
    mv "${dest}.part" "$dest"
    echo "fetched (${want_size}B): $dest" >&2
}

# abbr:PL-index-directory-name, in the order the index lists them (spec §2.3).
STATE_DIRS=(
    al:Alabama ak:Alaska az:Arizona ar:Arkansas ca:California co:Colorado
    ct:Connecticut de:Delaware dc:District_of_Columbia fl:Florida ga:Georgia
    hi:Hawaii id:Idaho il:Illinois in:Indiana ia:Iowa ks:Kansas ky:Kentucky
    la:Louisiana me:Maine md:Maryland ma:Massachusetts mi:Michigan
    mn:Minnesota ms:Mississippi mo:Missouri mt:Montana ne:Nebraska nv:Nevada
    nh:New_Hampshire nj:New_Jersey nm:New_Mexico ny:New_York nc:North_Carolina
    nd:North_Dakota oh:Ohio ok:Oklahoma or:Oregon pa:Pennsylvania
    ri:Rhode_Island sc:South_Carolina sd:South_Dakota tn:Tennessee tx:Texas
    ut:Utah vt:Vermont va:Virginia wa:Washington wv:West_Virginia wi:Wisconsin
    wy:Wyoming
)

fetch_pl() {
    need_free_mb 1434  # 1.4 GB, spec §2.5
    for pair in "${STATE_DIRS[@]}"; do
        local abbr="${pair%%:*}" dir="${pair#*:}"
        fetch_checked "$PL_BASE/$dir/${abbr}2020.pl.zip" "$DATA/pl/${abbr}2020.pl.zip"
    done
}

fetch_dhc_dc() {
    need_free_mb 1024  # DC's DHC zip alone is ~13 MB; 1 GB is a comfortable round number
    fetch_checked "$DHC_BASE/District_of_Columbia/dc2020.dhc.zip" "$DATA/dhc/dc2020.dhc.zip"
}

# The 1:20M states (~190 kB): no df check, it is smaller than any margin.
fetch_cb_states() {
    fetch_checked "$CB_BASE/cb_2020_us_state_20m.zip" "$DATA/cb/cb_2020_us_state_20m.zip"
}

fetch_tiger() {
    local fips="$1"
    need_free_mb "${GPUI_MAP_TIGER_NEED_MB:-2560}"  # 2.5 GB "in flight" (spec §2.5), or the converter's own figure
    fetch_checked "$TIGER_BASE/tl_2020_${fips}_tabblock20.zip" "$DATA/tiger/tl_2020_${fips}_tabblock20.zip"
}

# `[[ cond ]] && fn` as the script's very last statements would make the
# script's own exit code whatever the LAST such line's condition returned --
# `set -e`'s "exempt unless it's the last command in a && list" rule does
# not save this, because when a later flag is unset that test IS the last
# command actually run. A real run of `--pl --dhc-dc` (no `--tiger`) hit
# exactly this: every download succeeded, but the script still exited 1,
# because `[[ -n "$TIGER_FIPS" ]]` (false) was the last thing it ran. `if`
# blocks plus an explicit trailing `exit 0` avoid that.
if [[ $DO_PL -eq 1 ]]; then fetch_pl; fi
if [[ $DO_DHC_DC -eq 1 ]]; then fetch_dhc_dc; fi
if [[ $DO_CB_STATES -eq 1 ]]; then fetch_cb_states; fi
if [[ -n "$TIGER_FIPS" ]]; then fetch_tiger "$TIGER_FIPS"; fi
exit 0

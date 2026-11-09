#!/usr/bin/env bash
# The census performance regression gate. See docs/perf-gate.md.
#
#   scripts/perf-gate.sh [headless|windowed|all] [--update-baseline] [--reps N] [--max-load X]
#
# Builds the probe release binaries, checks the data, memory, disk and load,
# runs the tier(s) through examples/perf_gate.rs and prints a PASS/FAIL table.
# Exit: 0 pass, 1 regression, 2 not scored (no data / no valid run),
# 3 refused by a safety check, 4 build failure.
set -uo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TIER="${1:-headless}"
case "$TIER" in
  headless|windowed|all) shift || true ;;
  -*) TIER=headless ;;
  *) echo "usage: $0 [headless|windowed|all] [--update-baseline] [--reps N] [--max-load X]" >&2; exit 2 ;;
esac

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export RUSTC_WRAPPER=""
DATA="${US_BG_CACHE:-$HOME/.cache/gpui-map/demos/us-block-groups}"
LOGS="${PERF_GATE_LOGS:-${TMPDIR:-/tmp}/perf-gate}"

echo "== perf gate: $TIER ($(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null)) =="

# 1. Data: never invented, never substituted -- but skip the check when
#    `--only` names a section that never touches the census data at all
#    (`points` and `dots`/`dots.pan` run on seeded/procedural sources;
#    `sats`/`sats.run` on the seeded synthetic catalogue; `look` on
#    seeded synthetic textures;
#    `buildings` on vendored fixtures; `buildings.pan`/`buildings.pan_opaque`
#    on OpenFreeMap tiles; `story` on the vendored Singapore fixture tile).
ONLY=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--only" ]; then ONLY="$a"; fi
  prev="$a"
done
case "$ONLY" in
  points|dots|dots.pan|buildings|buildings.pan|buildings.pan_opaque|sats|sats.run|story|csv|explorer|mbtiles|fgb|geoparquet|cog|arcs|hexbin|trips|vtoverlay|night_race|look) ;;
  *)
    nshp=$(ls "$DATA"/shp/tl_2023_*_bg.shp 2>/dev/null | wc -l | tr -d ' ')
    if [ "$nshp" -lt 56 ] || [ ! -f "$DATA/acs/income_bg_2023.csv" ]; then
      echo "SKIP: census data missing ($nshp of 56 state shapefiles, income csv $( [ -f "$DATA/acs/income_bg_2023.csv" ] && echo present || echo missing )) under $DATA"
      echo "      fetch it with: demos/data/fetch_us_block_groups.sh   (public domain, ~1 GB, never committed)"
      exit 2
    fi
    ;;
esac

# 2. Disk: stop under 3 GB, on the target volume and the log volume.
mkdir -p "$CARGO_TARGET_DIR" "$LOGS"
for d in "$CARGO_TARGET_DIR" "$LOGS"; do
  avail=$(df -m "$d" | awk 'NR==2{print $4}')
  if [ "${avail:-0}" -lt 3000 ]; then
    echo "REFUSED: ${avail} MB free on the volume of $d (need >= 3 GB)"; exit 3
  fi
done

# 3. Memory: free + inactive must cover the projected peak plus 8 GB.
#    perf_gate re-checks before every run with the same mode-aware cap
#    (examples/perf_gate.rs's `Mode::projected_peak_mb`, MODES table).
#
#    The projected peak is mode-aware because the headless tier's own
#    sections top out far below the windowed sweep's, and a single blanket
#    number sized for the windowed sweep refused every headless-only run on
#    a quiet machine (free+inactive 11.5-13 GB here with nothing else
#    running, vs. a flat 14 GB floor). Measured max peak RSS per tier, from
#    real gate runs (`$LOGS/<mode>-<n>.log`, `gate: <mode>.peak_rss_mb` /
#    `.rss_leg_max_mb`), rounded up generously:
#      headless: national us_block_groups section 4468.7 MB is the highest
#        of the headless-tier sections (headless_states 537.1, points
#        778.9, dots 196.2 all well under it) -> round up to 5000 MB.
#      windowed/all: the windowed sweep tops out around 4648.7 MB (pan,
#        live, slider all near there; zoom_dc 3924.8, zoom_la 3505.9,
#        dots.pan 551 all lower) -- still under the original 6 GB
#        assumption, so the existing 14000 MB total (6000 + 8000) stands.
case "$TIER" in
  headless) projected_peak_mb=5000 ;;
  *)        projected_peak_mb=6000 ;;
esac
margin_mb=8000
# `memory_check PEAK_MB LABEL`: refuse (exit 3) unless free+inactive covers
# PEAK_MB + margin_mb.
memory_check() {
  local peak="$1" label="$2"
  local threshold_mb=$((peak + margin_mb))
  local page fi_mb
  page=$(vm_stat | sed -n 's/.*page size of \([0-9]*\) bytes.*/\1/p')
  fi_mb=$(vm_stat | awk -v p="$page" '/^Pages (free|inactive):/{gsub("\\.","",$NF); s+=$NF} END{printf "%d", s*p/1e6}')
  echo "memory: free+inactive ${fi_mb} MB | disk: $(df -h "$CARGO_TARGET_DIR" | awk 'NR==2{print $4}') free | load: $(sysctl -n vm.loadavg)"
  if [ "$fi_mb" -lt "$threshold_mb" ]; then
    echo "REFUSED: free+inactive ${fi_mb} MB < projected peak ${peak} MB + ${margin_mb} MB (${label})"; exit 3
  fi
}
# A full run is checked here, against the tier's worst mode. An `--only`
# run is checked after the build and copy (step 6b), against that one section's own
# peak, which only the runner's MODES table knows (`perf_gate
# --projected-peak`): the tier's worst refused `--only buildings` and
# `--only dots` on a machine with ~12.8 GB free.
if [ -z "$ONLY" ]; then
  memory_check "$projected_peak_mb" "$TIER tier"
fi

# Every demo measures uncached: a warm tile disk cache (the gallery default,
# `kit::with_tile_cache`) would time the disk instead of the network.
# perf_gate also sets this on each child it spawns.
export GPUI_MAP_TILE_CACHE=off

# 4. Build: the demos with the probe feature and the runner, in one go.
echo "building (release, --features probe,orbits) ..."
if ! cargo build -q --release --features probe,orbits --example us_block_groups --example big_points --example world_dots --example buildings_gate --example satellites --example story --example csv_points --example explorer_gate --example mbtiles --example fgb --example geoparquet --example cog --example arcs --example hexbin --example trips --example vtoverlay --example night_race_gate --example perf_gate \
     --manifest-path "$ROOT/Cargo.toml" 2> "$LOGS/build.log"; then
  tail -20 "$LOGS/build.log"; echo "BUILD FAILED (log $LOGS/build.log)"; exit 4
fi

# 5. Windowed tier: say what it needs. perf_gate refuses a covered,
#    unpainted, locked or loaded run itself; this is only the warning.
if [ "$TIER" != headless ]; then
  echo "windowed tier: a 1280x800 window will open and activate itself for each mode (~5 min)."
  echo "               Leave the screen unlocked and do not cover it; covered runs are refused, not scored."
fi

# 6. Run a PRIVATE copy of what step 4 built. `release/examples/<name>` is
#    the un-hashed link cargo re-points at whichever build of that example
#    finished last -- in a target dir other worktrees share, a sibling's
#    plain `cargo build --release --example us_block_groups` (no probe)
#    re-points it mid-run, and every probe-only row silently vanishes
#    (5239ac7's headless_states.table_only.* came back MISSING that way).
#    perf_gate finds the demos next to its own exe, so copying all seven is
#    enough.
BIN=$(mktemp -d "${TMPDIR:-/tmp}/perf-gate-bin.XXXXXX")
trap 'rm -rf "$BIN"' EXIT
for name in perf_gate us_block_groups big_points world_dots buildings_gate satellites story csv_points explorer_gate mbtiles fgb geoparquet cog arcs hexbin trips vtoverlay night_race_gate; do
  cp "$CARGO_TARGET_DIR/release/examples/$name" "$BIN/$name"
done

# 6b. An `--only` run's memory check, against its own section's peak (the
#     tier's figure if the runner does not know the name). Asked of the
#     private copy, not the shared target's link a sibling can re-point.
if [ -n "$ONLY" ]; then
  if only_peak=$("$BIN/perf_gate" --projected-peak "$ONLY" 2>/dev/null); then
    memory_check "$only_peak" "section $ONLY"
  else
    memory_check "$projected_peak_mb" "$TIER tier; no section named $ONLY"
  fi
fi
status=0
"$BIN/perf_gate" "$TIER" --log-dir "$LOGS" "$@" || status=$?
rm -rf "$BIN"
exit "$status"

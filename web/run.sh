#!/usr/bin/env bash
# Build the browser map and serve it. ONE command, from a clean checkout:
#
#     ./web/run.sh
#
# then open the URL it prints. `--no-build` skips the compile and serves
# whatever is already in `dist/`; a trailing number changes the port.
#
#     ./web/run.sh --no-build 8091
#
# What it needs, and what it does NOT:
#
#   * the `wasm32-unknown-unknown` target  (`rustup target add wasm32-unknown-unknown`)
#   * the `wasm-bindgen` CLI at the version `web/Cargo.toml` pins
#     (`cargo install wasm-bindgen-cli --version 0.2.127`)
#   * nothing else: the default tile source is OpenFreeMap's public,
#     keyless planet build (`?tiles=<template>` repoints it at any other
#     `{z}/{x}/{y}` endpoint, local or remote).
#
# No node, no bundler, no npm. The page is one HTML file, one JS glue file
# the `wasm-bindgen` CLI writes, and the module.
set -euo pipefail
cd "$(dirname "$0")"

BUILD=1
PORT=8090
for arg in "$@"; do
  case "$arg" in
    --no-build) BUILD=0 ;;
    [0-9]*) PORT="$arg" ;;
    *) echo "usage: $0 [--no-build] [port]" >&2; exit 2 ;;
  esac
done

if [ "$BUILD" = 1 ]; then
  ./build.sh
fi

# A reachability check on the default tile endpoint, BEFORE the browser is
# pointed at the page. "The map is blank" has three causes -- no reachable
# tile source, a hidden tab, and a real rendering fault -- and this rules
# out the first one here, where the answer is cheap, rather than in the
# console five minutes later. Tokyo Station's own z14 tile, the page's
# default camera -- see `web/src/app.rs`'s `DEFAULT_CENTER_LON`/`_LAT` and
# `TILE_URL` (`crate::basemap::OPENFREEMAP_PLANET_TILE_URL`).
TILE="https://tiles.openfreemap.org/planet/20260802_080001_pt/14/14552/6451.pbf"
if code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 4 "$TILE") && [ "$code" = 200 ]; then
  echo "tiles:              tiles.openfreemap.org answering (HTTP $code)"
else
  echo "tiles:              tiles.openfreemap.org NOT answering (no network, or the dated" >&2
  echo "                    build path 404s -- see OPENFREEMAP_PLANET_TILE_URL's doc comment)," >&2
  echo "                    or pass ?tiles=<template> for another source" >&2
fi
# The census demo (`?census=`): its block-group pack is built data, never
# committed, read from the directory `serve.py` serves `/census/` from.
PACK="${GPUI_MAP_CENSUS_DIR:-$HOME/.cache/gpui-map-data/census-web}/us_bg_2023.bgpk"
if [ -s "$PACK" ]; then
  echo "census:             http://127.0.0.1:$PORT/?census=us_bg_2023 (needs ~/.cache/gpui-map-data/census-web/us_bg_2023.bgpk; see tools/census-pack)"
else
  echo "census:             no pack at $PACK; build it with"
  echo "                    cargo run --release --manifest-path tools/census-pack/Cargo.toml -- --out $PACK"
fi
echo
exec python3 serve.py "$PORT"

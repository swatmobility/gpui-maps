#!/usr/bin/env bash
# Downloads the space look's two textures into ~/.cache/gpui-map/space/
# (or $GPUI_MAP_SPACE_DIR) and checks them against the sha256 digests pinned
# in src/space_assets.rs. The engine does the same on first use; this is the
# by-hand version, e.g. before a headless `globe_capture --space-look`.
#
#     ./scripts/fetch-space-assets.sh
#
# A copy of both files is committed in web/space/ (the site serves them
# anyway): a file there with the pinned digest is used before the network,
# so the site build does not depend on NASA's server, which GitHub's runners
# cannot always reach. The download stays for a checkout without them.
#
# What the digests pin: the UPSTREAM bytes, as served. Both files are used
# as-is -- the star map is Wikimedia Commons' 3840x1920 rendition of the SVS
# 64k original, the clouds are NASA's own 2048x1024 JPEG -- so no local
# downscale is needed, and any size limit (WebGL2's 2048) is met at upload
# time by the engine's box filter, after verification. A local re-encode
# (sips/ffmpeg) is not bit-reproducible across tool versions, which is why
# the processed output is not what is pinned.
#
# KNOWN RISK: the star map is a Commons THUMBNAIL, which Wikimedia may
# regenerate with different bytes; the pin then fails and the look draws the
# plain sky. The stable sources were unusable on 2026-09-30: svs.gsfc.nasa.gov
# did not answer (and its 4k maps are OpenEXR), and the Commons original is a
# 541 MB 32768x16384 JPEG. Fix: host a copy (redistribution with attribution
# is allowed) and point this URL and src/space_assets.rs at it.
#
# Credits (they join the map's attribution while drawn):
#   Stars: "NASA/Goddard Space Flight Center Scientific Visualization
#          Studio. Gaia DR2: ESA/Gaia/DPAC." -- Deep Star Maps 2020
#          (https://svs.gsfc.nasa.gov/4851). Attribution required.
#   Clouds: NASA Visible Earth, Blue Marble: Clouds
#          (https://visibleearth.nasa.gov/images/57747); NASA Goddard Space
#          Flight Center image by Reto Stoeckli.
set -euo pipefail

dir="${GPUI_MAP_SPACE_DIR:-$HOME/.cache/gpui-map/space}"
# The project, never a person: no email, no token.
ua="gpui-map-space-look (+https://github.com/swatmobility/gpui-maps)"
mkdir -p "$dir"
seed="$(cd "$(dirname "$0")/.." && pwd)/web/space"

fetch() {
  local file="$1" url="$2" sha="$3"
  local out="$dir/$file"
  if [[ -f "$out" ]] && [[ "$(shasum -a 256 "$out" | cut -d' ' -f1)" == "$sha" ]]; then
    echo "ok (cached)  $out"
    return
  fi
  if [[ -f "$seed/$file" ]] && [[ "$(shasum -a 256 "$seed/$file" | cut -d' ' -f1)" == "$sha" ]]; then
    cp "$seed/$file" "$out"
    echo "ok (in repo) $out"
    return
  fi
  curl -sSfL --retry 5 --retry-all-errors --retry-delay 5 -m 300 -A "$ua" -o "$out.part" "$url"
  local got
  got="$(shasum -a 256 "$out.part" | cut -d' ' -f1)"
  if [[ "$got" != "$sha" ]]; then
    rm -f "$out.part"
    echo "sha256 mismatch for $file: got $got, want $sha (upstream changed?)" >&2
    exit 1
  fi
  mv "$out.part" "$out"
  echo "ok (fetched) $out"
}

fetch starmap_2020_3840.jpg \
  "https://upload.wikimedia.org/wikipedia/commons/thumb/a/a9/Deep_Star_Maps_2020_%E2%80%93_Starmap_2020_64k.jpg/3840px-Deep_Star_Maps_2020_%E2%80%93_Starmap_2020_64k.jpg" \
  1b5dd6f7e3dd959a742f35dde1d95966fbae14140ef29d1f648f1d0b6af0bfbc
fetch cloud_combined_2048.jpg \
  "https://eoimages.gsfc.nasa.gov/images/imagerecords/57000/57747/cloud_combined_2048.jpg" \
  daddaad84d7a33bbbc86cdda3f591099f57cee8607b7bcf3b67eb7e4f7a1c793

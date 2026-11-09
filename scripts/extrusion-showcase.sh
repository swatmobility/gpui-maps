#!/usr/bin/env bash
# Pitched-city captures of lit 3D buildings on OpenFreeMap Liberty (no key),
# each checked against three controls:
#   - buildings OFF (--layer-off building-3d): the layer must change >= 2% of
#     the frame (layer_px);
#   - buildings FLAT (--extrusion-flat, roofs at ground level): rising must
#     change >= 1% (wall_px) -- a layer that compiles but does not rise fails.
#     wall_px is roofs moving AND walls appearing, so it proves "rises", not
#     "has walls"; wall_px / layer_px >= 0.60 (see RATIO_MIN) is the check
#     that the rise is most of what the layer draws;
#   - sentinel clear, buildings on vs off: no magenta in the bottom 60% that is
#     NEW with buildings on (new_magenta). This catches ONLY a pass that erases
#     painted ground (a fill or line returned to the clear colour). Liberty
#     paints bare land with its `background` layer, which the sentinel clear
#     replaces, so bare land is magenta in both frames and a hole over bare
#     land -- most of the ground buildings stand on -- is invisible to it.
#     ground_magenta (the raw count, i.e. bare land) is printed for information.
#     layer_px, wall_px and reading the frames are the guards there.
# Also writes $name-opaque.png (--extrusion-opacity 1), the same camera with
# Liberty's 0.8 translucency off, as an alternative look; it is not gated.
#
# Run from the repo root (it uses scripts/, examples/ and cargo relative to it):
#   scripts/extrusion-showcase.sh <out dir>
# It builds the release globe_capture itself (a no-op when fresh) and copies it
# into <out dir>/bin, so a sibling worktree relinking the shared target dir
# mid-run cannot swap the binary under it.
set -u
OUT=${1:?out dir}; mkdir -p "$OUT/bin"
[ -f scripts/png-diff.py ] && [ -f examples/liberty-style.json ] || { echo "run from the repo root"; exit 2; }
TARGET=${CARGO_TARGET_DIR:-$(pwd)/target}
CARGO_TARGET_DIR=$TARGET timeout 1500 cargo build --release --example globe_capture || { echo "globe_capture build failed"; exit 3; }
cp -p "$TARGET/release/examples/globe_capture" "$OUT/bin/globe_capture" || exit 3
BIN=$OUT/bin/globe_capture
COMMON="--flat --zoom 16 --pitch 60 --bearing 30 --size 1024 --labels --style examples/liberty-style.json --source openfreemap --cache $OUT/tiles"
FRAME=$((1024 * 1024))
# Measured 2026-09-29 (b01615d): 0.782 Shinjuku, 0.857 Manhattan, 0.834
# Singapore. 0.60 sits ~0.18 under the lowest, room for tile data and style
# drift, while a frame whose buildings stop rising over a large share of the
# layer (lost walls, heights collapsing, whole tiles flattened) falls below it.
RATIO_MIN=60   # percent
status=0
for city in "shinjuku 139.6995 35.6905" "manhattan -73.9857 40.7484" "singapore 103.8515 1.2830"; do
  set -- $city; name=$1; at="--lon $2 --lat $3"
  rm -f "$OUT/$name.png" "$OUT/$name-off.png" "$OUT/$name-flat.png" "$OUT/$name-sentinel.png" \
        "$OUT/$name-sentinel-off.png" "$OUT/$name-opaque.png"
  failed=""
  cap() { local file=$1; shift; timeout 1500 "$BIN" $COMMON $at "$@" --out "$OUT/$file" || failed="$failed $file"; }
  cap "$name.png"
  cap "$name-off.png" --layer-off building-3d
  cap "$name-flat.png" --extrusion-flat
  cap "$name-sentinel.png" --clear sentinel --no-backdrop
  cap "$name-sentinel-off.png" --clear sentinel --no-backdrop --layer-off building-3d
  cap "$name-opaque.png" --extrusion-opacity 1
  if [ -n "$failed" ]; then echo "$name: capture failed:$failed FAIL"; status=1; continue; fi
  off=$(scripts/png-diff.py "$OUT/$name.png" "$OUT/$name-off.png" --tolerance 2 | sed 's/ .*//;s/differing=//')
  walls=$(scripts/png-diff.py "$OUT/$name.png" "$OUT/$name-flat.png" --tolerance 2 | sed 's/ .*//;s/differing=//')
  magenta=$(scripts/png-diff.py --count 255,0,255,255 "$OUT/$name-sentinel.png" --rows 410,1024 | sed 's/matching=//')
  holes=$(scripts/png-diff.py --count-new 255,0,255,255 "$OUT/$name-sentinel.png" "$OUT/$name-sentinel-off.png" --rows 410,1024 | sed 's/new=//')
  ratio=$(( off > 0 ? walls * 1000 / off : 0 ))
  verdict=PASS
  [ "$off" -ge $((FRAME / 50)) ] || verdict=FAIL
  [ "$walls" -ge $((FRAME / 100)) ] || verdict=FAIL
  [ "$ratio" -ge $((RATIO_MIN * 10)) ] || verdict=FAIL
  [ "$holes" -eq 0 ] || verdict=FAIL
  printf '%s: layer_px=%s wall_px=%s ratio=%d.%03d ground_magenta=%s new_magenta=%s %s\n' \
    "$name" "$off" "$walls" $((ratio / 1000)) $((ratio % 1000)) "$magenta" "$holes" "$verdict"
  [ $verdict = PASS ] || status=1
done
exit $status

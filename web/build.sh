#!/usr/bin/env bash
# Builds the spike and lays out `dist/` for any static server.
#
# No `wasm-pack`/`trunk`: `cargo build` plus the `wasm-bindgen` CLI is the
# whole toolchain, and keeping it to those two makes the bundle-size number
# below attributable to the crate rather than to a bundler's defaults.
set -euo pipefail
cd "$(dirname "$0")"

# `--viewer`: the bundle a page embedding the map ships (a downstream share
# viewer copies it): the module, styles, fonts, sprites and their licences,
# without this page's own index.html, sweep/census scripts and route
# fixtures, plus a VERSION stamp (commit and wasm hash) so an embedder can
# pin and check what it copied.
VIEWER=0
for arg in "$@"; do
  case "$arg" in
    --viewer) VIEWER=1 ;;
    *) echo "unknown argument: $arg (known: --viewer)" >&2; exit 2 ;;
  esac
done

# Mapbox is OFF unless this build opts in with GPUI_MAP_ALLOW_MAPBOX=1
# (2026-10, a usage spike on the key). Without the opt-in no token is baked
# -- MAPBOX_ACCESS_TOKEN is dropped from cargo's environment -- and
# `app.rs::MAPBOX_ALLOWED` is false, so a `?access_token=` is ignored too.
if [ "${GPUI_MAP_ALLOW_MAPBOX:-}" = "1" ]; then
  export GPUI_MAP_ALLOW_MAPBOX=1
  echo "mapbox opt-in:      GPUI_MAP_ALLOW_MAPBOX=1 -- a MAPBOX_ACCESS_TOKEN, if set, is baked in"
else
  unset MAPBOX_ACCESS_TOKEN GPUI_MAP_ALLOW_MAPBOX
  echo "mapbox opt-in:      off -- no token baked; the page draws OpenFreeMap (set GPUI_MAP_ALLOW_MAPBOX=1 to opt in)"
fi

cargo build --release --target wasm32-unknown-unknown

# ASK cargo where it put the thing, rather than assuming `web/target/`.
#
# This said `target/wasm32-unknown-unknown/release/gpui_maps_web.wasm`, on
# the stated grounds that the default `web/target/` is gitignored. It is --
# but the default is not the only possibility, and a `[build] target-dir` in
# any `.cargo/config.toml` at or ABOVE this directory silently redirects it.
# Cargo walks parents to find that file, so a checkout whose repository root
# carries one (pointing at an external cache, say) builds successfully and
# then fails here with
#
#     error: failed reading 'target/wasm32-unknown-unknown/release/gpui_maps_web.wasm'
#     Caused by: No such file or directory (os error 2)
#
# which reads like a compile failure and is not one. `cargo metadata` knows
# the answer in every configuration; python3 is already a dependency of this
# script (the sprite-entry count below).
TARGET_DIR="$(cargo metadata --format-version 1 --no-deps \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
WASM="$TARGET_DIR/wasm32-unknown-unknown/release/gpui_maps_web.wasm"
[ -s "$WASM" ] || { echo "BUILD FAILED: $WASM missing or empty" >&2; exit 1; }

rm -rf dist && mkdir -p dist
wasm-bindgen --target web --no-typescript \
  --out-dir dist \
  "$WASM"
cp index.html sweep.js dist/
# `?census=`: the page's DOM half, and the census demo's keyless basemap
# (Positron), verbatim from demos/. Its Mapbox Light basemap is Mapbox's
# document and not in the tree: copied below from $MAPBOX_FIXTURES, with a
# token only.
cp census.js dist/
# The census page's data (`?census=us_bg_2023`): the pack `tools/census-pack`
# builds, 41 MB and not in the tree. Copied when there is one
# (`$GPUI_MAP_CENSUS_PACK`, else the cache); without it the page has nothing
# to load and the launch site drops its census tab.
CENSUS_PACK="${GPUI_MAP_CENSUS_PACK:-$HOME/.cache/gpui-map-data/census-web/us_bg_2023.bgpk}"
if [ -s "$CENSUS_PACK" ]; then
  mkdir -p dist/census && cp "$CENSUS_PACK" dist/census/
fi
cp ../demos/positron-style.json dist/
# The committed route fixtures, for `?nav=sim&route=<name>` (`nav::demo`).
# Real OSRM responses with the key already stripped (tests/fixtures/routes/,
# written only by `examples/fetch_route_fixture.rs`, which refuses to save
# bytes that still contain it). This build takes no routing key at all.
mkdir -p dist/routes && cp ../tests/fixtures/routes/*.json dist/routes/
# The list is `nav::demo::BUILTIN_ROUTES`, read from the source rather than
# repeated here, so a fixture added there cannot be missing from dist/.
ROUTES_LINE="$(grep -E '^pub const BUILTIN_ROUTES: \[&str; [0-9]+\] = \[' ../src/nav/demo.rs || true)"
ROUTES_LEN="$(printf '%s' "$ROUTES_LINE" | sed -E 's/.*\[&str; ([0-9]+)\].*/\1/')"
ROUTES="$(printf '%s' "$ROUTES_LINE" | sed -E 's/^[^=]*= *//' | grep -oE '"[a-z0-9-]+"' | tr -d '"' || true)"
if [ -z "$ROUTES" ] || [ "$(printf '%s\n' "$ROUTES" | wc -l | tr -d ' ')" != "$ROUTES_LEN" ]; then
  echo "BUILD FAILED: could not read BUILTIN_ROUTES from src/nav/demo.rs (one line, [&str; N])" >&2
  exit 1
fi
for f in $ROUTES ; do
  [ -s "dist/routes/$f.json" ] || { echo "BUILD FAILED: dist/routes/$f.json missing or empty" >&2; exit 1; }
done
# A fixture that somehow kept its `access_token` would publish it to every
# page load: refuse to ship one.
if grep -l 'access_token=' dist/routes/*.json >/dev/null 2>&1; then
  echo "BUILD FAILED: a route fixture in dist/routes/ still carries an access_token" >&2
  exit 1
fi
# The style documents, verbatim from the shipped crate's examples -- the page
# fetches one rather than embedding it. `open-streets-style.json` (our
# Mapbox Streets look-alike on OpenFreeMap) is what `app.rs::STYLE_URL`
# names; the others are copied so `?style=` can switch between them without
# a rebuild, which is the whole point of that override.
cp ../examples/styles-open/open-streets-style.json ../examples/styles-open/open-dark-style.json \
   ../examples/styles-open/open-swat-routes-dark-style.json ../examples/liberty-style.json dist/

# The space look's textures, for `?look=space` (`app::load_space_textures`):
# NASA's star map and cloud map, attribution required, committed in
# `web/space/`. `scripts/fetch-space-assets.sh` copies them (or, without
# that copy, downloads them) into the cache
# (`$GPUI_MAP_SPACE_DIR`, else `~/.cache/gpui-map/space/`) and checks each
# against the sha256 pinned in `src/space_assets.rs`; a file already cached
# with the right digest is not downloaded again. So a clean CI runner fetches
# them once, and a mismatch (an upstream re-encode) fails the build here
# rather than shipping bytes nobody checked. Their credits join the page's
# attribution while drawn (`space_look::look_credits`).
SPACE_DIR="${GPUI_MAP_SPACE_DIR:-$HOME/.cache/gpui-map/space}"
GPUI_MAP_SPACE_DIR="$SPACE_DIR" bash ../scripts/fetch-space-assets.sh
mkdir -p dist/space
for f in starmap_2020_3840.jpg cloud_combined_2048.jpg ; do
  cp "$SPACE_DIR/$f" dist/space/
  [ -s "dist/space/$f" ] || { echo "BUILD FAILED: dist/space/$f missing or empty" >&2; exit 1; }
done

# Mapbox Streets over the real Mapbox `composite` source -- the map native's
# `examples/mapbox_streets_demo.rs` and iOS draw -- but ONLY when this build
# was given a token:
#
#     GPUI_MAP_ALLOW_MAPBOX=1 MAPBOX_ACCESS_TOKEN="$TOK" ./web/build.sh
#
# `app.rs::BAKED_MAPBOX_ACCESS_TOKEN` is `option_env!`, so the token reaches
# the wasm through the compiler's environment and lands in `dist/` (which is
# gitignored) and nowhere else -- never in a tracked file, never printed
# here. Without the variable the public build is exactly what it was: no
# Mapbox style in `dist/`, no token in the module, OpenMapTiles only.
#
# A `pk.` token is a public client token by Mapbox's design, but a dist
# built with one gives it to anyone who can load the page -- a tunnel
# included.
#
# The Mapbox style documents are Mapbox's and not in the tree: they come
# from the directory `MAPBOX_FIXTURES` names (`scripts/fetch-mapbox-styles.sh`
# fills one). A token with no fixtures still builds; the page logs that
# Streets is not in this dist and falls back to the keyless style.
if [ -n "${MAPBOX_ACCESS_TOKEN:-}" ]; then
  echo "mapbox:             token baked in (not shown)"
  for f in mapbox-streets-style.json mapbox-light-style.json; do
    if [ -n "${MAPBOX_FIXTURES:-}" ] && [ -s "$MAPBOX_FIXTURES/$f" ]; then
      cp "$MAPBOX_FIXTURES/$f" dist/
      echo "mapbox:             $f copied from \$MAPBOX_FIXTURES"
    else
      echo "mapbox:             $f not found (set MAPBOX_FIXTURES; see scripts/fetch-mapbox-styles.sh) -- keyless fallback"
    fi
  done
else
  echo "mapbox:             no MAPBOX_ACCESS_TOKEN -- OpenMapTiles styles only (the public build)"
fi

# The label faces, and the licence that lets them be redistributed.
#
# Noto Sans JP, subset to the 2,836 codepoints that actually occur in
# a Japan map-feature table -- see `app::FONT_URLS` for where that number comes
# from and what the alternatives cost. Fetched by the page rather than
# embedded in the module, so they stay off the critical path to first paint
# and cache independently of a rebuild.
cp ../assets/fonts/NotoSansJP-Regular.japan-subset.otf \
   ../assets/fonts/NotoSansJP-Bold.japan-subset.otf \
   ../assets/fonts/NotoSans-Regular.latin-subset.ttf \
   ../assets/fonts/NotoSans-Bold.latin-subset.ttf \
   ../assets/fonts/LICENSE-OFL.txt dist/
# The OFL requires the licence to travel with the fonts. A build that would
# ship them without it fails here, for the same reason the attribution check
# below exists: a licence breach is a defect, not a cosmetic omission.
for f in NotoSansJP-Regular.japan-subset.otf NotoSansJP-Bold.japan-subset.otf \
          NotoSans-Regular.latin-subset.ttf NotoSans-Bold.latin-subset.ttf \
          LICENSE-OFL.txt ; do
  [ -s "dist/$f" ] || { echo "BUILD FAILED: dist/$f missing or empty" >&2; exit 1; }
done
# The OFL notice travels with the fonts, and it has to name the copyright
# holder of EACH of them -- the two pairs have different ones (see
# `app.rs::FONT_URLS`). A notice that lost one of these lines would be
# shipping a font whose copyright statement is not in the file beside it,
# which is the breach the check above only half prevents.
for holder in 'Adobe' 'Google Inc.' ; do
  grep -qF -- "$holder" dist/LICENSE-OFL.txt || {
    echo "BUILD FAILED: dist/LICENSE-OFL.txt does not name the copyright holder: $holder" >&2
    echo "  Both font pairs ship under OFL 1.1 but with different holders." >&2
    exit 1
  }
done
# The Latin pair is not decoration: without its capitals every place name
# outside Japan loses its initials and every expressway shield goes blank.
# A subset regenerated without `U+0041`-`U+005A` would fail silently, the
# way the Japan subset did for months, so the coverage is asserted on the
# SHIPPED file rather than trusted. Vietnamese likewise: a subset cut to
# Latin Extended-A drew "Thu Dau Mot" with blanks for every horned vowel
# (U+01A0/01A1/01AF/01B0) and every stacked-diacritic letter
# (U+1EA0-1EF9), so the full Vietnamese alphabet is asserted too.
python3 - <<'PYEOF' || exit 1
import string, sys
from fontTools.ttLib import TTFont
for weight in ('Regular', 'Bold'):
    path = 'dist/NotoSans-%s.latin-subset.ttf' % weight
    covered = set(TTFont(path).getBestCmap())
    wanted = set(string.ascii_uppercase) | set('bdghkmqvz')
    wanted |= {chr(c) for c in (0x01A0, 0x01A1, 0x01AF, 0x01B0)}
    wanted |= {chr(c) for c in range(0x1EA0, 0x1EFA)}
    missing = sorted(c for c in wanted if ord(c) not in covered)
    if missing:
        sys.stderr.write('BUILD FAILED: %s is missing %s\n' % (path, ''.join(missing)))
        sys.exit(1)
PYEOF

# The basemap's icons: route shields, one-way arrows, POI markers. OUR sheet,
# generated by `sprites/build_sprites.py` -- shields drawn here, POI glyphs
# from the CC0 maki set. NOT Mapbox's `mapbox://sprites/mapbox/streets-v12`,
# which the style document names and which this build deliberately does not
# use: that art is not ours to redistribute and this repository is public,
# and resolving that reference at runtime would mean baking an access token
# into a page. See `sprites/README.md` and `app.rs::SPRITE_URL`. (A build
# given MAPBOX_ACCESS_TOKEN above fetches Mapbox's sheet at RUN time for
# Mapbox Streets only, as native does -- nothing of it is copied here.)
#
# `@2x` in the filename is not decoration -- `sprite::sprite_urls` appends
# `@2x.json`/`@2x.png` to the base URL, so these two names are what the page
# actually asks for.
cp sprites/'japan-sprite@2x.png' sprites/'japan-sprite@2x.json' dist/
cp sprites/maki/LICENSE.txt dist/LICENSE-MAKI.txt
# Same rule as the fonts above: a build that would ship the icons without
# the licence they came under fails here. CC0 waives the requirement to
# carry it, which is a reason to ship it anyway rather than a reason not to
# -- provenance is what makes the waiver checkable.
for f in 'japan-sprite@2x.png' 'japan-sprite@2x.json' LICENSE-MAKI.txt ; do
  [ -s "dist/$f" ] || { echo "BUILD FAILED: dist/$f missing or empty" >&2; exit 1; }
done
# The sheet has to carry the names the style's shield expressions resolve to,
# and a sheet regenerated without them would fail silently -- every shield
# would simply stop drawing, which is exactly what this build looked like
# before it had a sheet at all. Checked here rather than discovered in a
# screenshot.
for name in jp-expressway-2 jp-expressway-3 jp-national-route-2 jp-national-route-3 \
            jp-prefectural-road-2 jp-prefectural-road-3 default-2 default-3 \
            oneway-small oneway-large ; do
  grep -q "\"$name\"" "dist/japan-sprite@2x.json" || {
    echo "BUILD FAILED: the sprite sheet has no \"$name\" -- shields will not draw" >&2
    exit 1
  }
done

# The attribution is a condition of using this data, and `dist/index.html` is
# a copy -- so this checks the COPY, after it has been made, rather than the
# source it came from.
#
# It used to grep for one source's two credit lines literally. It cannot any
# more, and the reason is the defect that change fixed: `?tiles=` repoints
# this build at any endpoint, and a credit frozen into the markup is then
# simply false -- pointed at OpenFreeMap the page asserted a commercial
# licence over OpenStreetMap data and omitted the ODbL credit that data
# requires. The lines now live in `app.rs::attribution_for`, keyed by source,
# and `app::render_attribution` writes them into the container below at
# startup.
#
# So what this checks is the container. Lose it and the credit has nowhere to
# go and the map draws without one, which is exactly the failure the old
# check existed to prevent -- it is the same gate, one level up.
grep -qF -- 'id="credit-lines"' dist/index.html || {
  echo "BUILD FAILED: dist/index.html has no #credit-lines container --" >&2
  echo "  the map would draw with NO attribution. See app::render_attribution." >&2
  exit 1
}
# And the source half: the credits themselves must still be in the module the
# page reads them from. A `?tiles=` the table does not know falls through to
# `UNKNOWN_SOURCE_CREDIT`, which claims nothing on anyone's behalf -- and
# that now includes the DEFAULT endpoint, a private machine-local corpus this
# repository does not distribute and cannot check (see `attribution_for`).
# What must not silently vanish is the ODbL credit for the one PUBLIC source
# the table names, and the honest fallback itself: lose either and a page
# that does have a named obligation would draw without it.
for needle in 'OpenStreetMap contributors' 'UNKNOWN_SOURCE_CREDIT' ; do
  grep -qF -- "$needle" src/app.rs || {
    echo "BUILD FAILED: web/src/app.rs no longer carries the credit: $needle" >&2
    exit 1
  }
done

echo
echo "raw cargo output:   $(du -h "$WASM" | cut -f1)"
echo "wasm-bindgen wasm:  $(du -h dist/gpui_maps_web_bg.wasm | cut -f1)"
echo "js glue:            $(du -h dist/gpui_maps_web.js | cut -f1)"
echo "style json:         $(du -h dist/open-streets-style.json | cut -f1)"
echo "fonts:              $(du -ch dist/NotoSansJP-*.otf dist/NotoSans-*.ttf | tail -1 | cut -f1) over 4 files (2 Japan, 2 Latin)"
echo "sprites:            $(du -ch dist/'japan-sprite@2x.png' dist/'japan-sprite@2x.json' | tail -1 | cut -f1) over 2 files, \
$(python3 -c "import json,sys;print(len(json.load(open(sys.argv[1]))))" "dist/japan-sprite@2x.json") entries"

if [ "$VIEWER" = 1 ]; then
  rm -rf dist/index.html dist/sweep.js dist/census.js dist/routes
  {
    echo "gpui-map web viewer bundle"
    echo "commit: $(git rev-parse --short HEAD 2>/dev/null || echo unknown)$(git diff --quiet HEAD -- . ../src 2>/dev/null || echo ' (dirty)')"
    echo "wasm sha256: $(shasum -a 256 dist/gpui_maps_web_bg.wasm | cut -d' ' -f1)"
    echo "exports: $(grep -oE 'export function map_[a-z_]+' dist/gpui_maps_web.js | sed 's/export function //' | sort | tr '\n' ' ')"
  } > dist/VERSION
  echo "viewer bundle:      dist/ (page files dropped), dist/VERSION written"
fi

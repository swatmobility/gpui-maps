#!/usr/bin/env bash
# The one clippy command: what the pre-commit hook runs, and what a human
# runs before pushing. Every lint is an error (`-D warnings`); the handful
# of clippy defaults this code deliberately departs from are configured,
# each with its reason, under `[lints.clippy]` in Cargo.toml (and repeated
# in web/Cargo.toml and ios/Cargo.toml, which compile the same src/ files).
#
#     ./scripts/clippy.sh            # root, --all-targets, orbits,clickhouse-demo
#     ./scripts/clippy.sh --web      # ... plus the browser shell (wasm32)
#     ./scripts/clippy.sh --ios      # ... plus the iOS shell (aarch64-apple-ios) and embed-ios/
#     ./scripts/clippy.sh --all      # all of the above, plus root default features
#                                    # and the consumer set (probe,sprite_http_fetch)
#
# This file lints the LIBRARY crates only (the ones that go to the open
# gpui-maps repo at the split). The Studio crates -- gpui-map-dash-ui/,
# gpui-agentic-acp/, studio-mobile/ -- are linted by scripts/clippy-studio.sh,
# which this script runs at the end with the same --ios/--all flags, so one
# command still lints everything while both live in one repo. After the
# split clippy-studio.sh is absent here and that step is skipped; set
# GPUI_MAP_CLIPPY_LIB_ONLY=1 to skip it now.
#
# Why these targets: `web/` and `ios/` are their own workspaces (see
# scripts/check-wasm.sh), so a root clippy run never lints them, and a few
# lints fire only on their targets (wgpu handles are `!Send` on wasm32).
# The shells are linted `--lib` only, for the reason check-wasm.sh gives.
# Default features are in `--all` because some code is cfg'd on `orbits`
# and lints differently without it.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${CARGO_TARGET_DIR:=$PWD/target}"
export CARGO_TARGET_DIR RUSTC_WRAPPER="" CARGO_INCREMENTAL=0

web=0 ios=0 default=0
for arg in "$@"; do
  case "$arg" in
    --web) web=1 ;;
    --ios) ios=1 ;;
    --all) web=1 ios=1 default=1 ;;
    -h|--help) sed -n '2,27p' "$0"; exit 0 ;;
    *) echo "clippy.sh: unknown argument '$arg' (try --help)" >&2; exit 2 ;;
  esac
done

need_target() {
  if ! rustup target list --installed 2>/dev/null | grep -qx "$1"; then
    echo "clippy.sh: the $1 target is NOT installed -- not linted, and that is not a pass." >&2
    echo "clippy.sh:   rustup target add $1" >&2
    exit 2
  fi
}

echo "clippy.sh: root --all-targets --features orbits,clickhouse-demo"
cargo clippy --quiet --all-targets --features orbits,clickhouse-demo -- -D warnings

# The Explorer's pure document crate: its own workspace, so the root run
# above never lints it. Cheap (serde only), so always.
# The pass above is the FREE Studio (no Studio feature flags); this
# one lints the full internal Studio, whose cfg'd-in code the free pass
# never sees.
# Skipped in the open library cut, which has no Studio example.
if [ "${GPUI_MAP_CLIPPY_LIB_ONLY:-0}" != 1 ] && [ -d examples/studio ]; then
  echo "clippy.sh: root --example studio --features orbits,clickhouse-demo,studio-full"
  cargo clippy --quiet --example studio --features orbits,clickhouse-demo,studio-full -- -D warnings
fi

echo "clippy.sh: kit/ --all-targets --all-features"
cargo clippy --quiet --all-targets --all-features --manifest-path kit/Cargo.toml -- -D warnings
echo "clippy.sh: swat-maps-doc/ --all-targets"
cargo clippy --quiet --all-targets --manifest-path swat-maps-doc/Cargo.toml -- -D warnings

# The dashboard framework's pure crate: its own workspace, every feature
# (http-native and http-web both compile natively; fetch is a stub there).
echo "clippy.sh: swat-maps-dash/ --all-targets --all-features"
cargo clippy --quiet --all-targets --all-features --manifest-path swat-maps-dash/Cargo.toml -- -D warnings

if [ "$default" = 1 ]; then
  echo "clippy.sh: root --all-targets (default features)"
  cargo clippy --quiet --all-targets -- -D warnings
fi
if [ "$default" = 1 ]; then
  # The feature set downstream apps pin (e.g. `default-features =
  # false, features = ["probe", "sprite_http_fetch"]`): the library alone,
  # no mbtiles/geoparquet. `basemaps` must build here.
  echo "clippy.sh: root --lib --no-default-features --features probe,sprite_http_fetch (consumer set)"
  cargo clippy --quiet --lib --no-default-features --features probe,sprite_http_fetch -- -D warnings
fi
if [ "$web" = 1 ]; then
  need_target wasm32-unknown-unknown
  echo "clippy.sh: web/ --target wasm32-unknown-unknown --lib"
  (cd web && cargo clippy --quiet --target wasm32-unknown-unknown --lib -- -D warnings)
fi
if [ "$ios" = 1 ]; then
  need_target aarch64-apple-ios
  echo "clippy.sh: ios/ --target aarch64-apple-ios --lib"
  (cd ios && cargo clippy --quiet --target aarch64-apple-ios --lib -- -D warnings)
  # Without the winit shell: what the embeddable engine links.
  echo "clippy.sh: ios/ --target aarch64-apple-ios --lib --no-default-features"
  (cd ios && cargo clippy --quiet --target aarch64-apple-ios --lib --no-default-features -- -D warnings)
  # The embeddable engine (`embed-ios/`, its own workspace): the iOS lib,
  # and its headless tests on the host.
  echo "clippy.sh: embed-ios/ --target aarch64-apple-ios --lib"
  (cd embed-ios && cargo clippy --quiet -p gpui-maps-embed --target aarch64-apple-ios --lib -- -D warnings)
  echo "clippy.sh: embed-ios/ --all-targets (host)"
  (cd embed-ios && cargo clippy --quiet -p gpui-maps-embed --all-targets -- -D warnings)
fi
echo "clippy.sh: library crates OK -- no warnings."

# The Studio crates, while they still live in this repository.
if [ "${GPUI_MAP_CLIPPY_LIB_ONLY:-0}" != 1 ] && [ -f scripts/clippy-studio.sh ]; then
  studio_args=()
  [ "$ios" = 1 ] && studio_args+=(--ios)
  bash scripts/clippy-studio.sh ${studio_args[@]+"${studio_args[@]}"}
fi
echo "clippy.sh: OK -- no warnings."

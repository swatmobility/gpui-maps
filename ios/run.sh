#!/usr/bin/env bash
# Builds the spike, wraps it in a `.app`, installs it on a booted simulator
# and launches it with the console attached.
#
# No `xcodebuild`, no `cargo-bundle`, no `xcrun -sdk`: an iOS app bundle for
# the SIMULATOR is a directory containing an `Info.plist` and a Mach-O built
# for `aarch64-apple-ios-sim`, and `simctl` asks for nothing else -- no code
# signature, no provisioning profile, no entitlements.
#
#   ./run.sh              build, bundle, install, launch (console attached)
#   ./run.sh --no-launch  build, bundle, install only
#
# SIMULATOR ONLY, deliberately. `aarch64-apple-ios` (the device target)
# builds a second, equally large target tree and cannot be run here, so this
# script never names it.
set -euo pipefail
cd "$(dirname "$0")"

TARGET=aarch64-apple-ios-sim
# `cargo build`, not `--release`. See `[profile.dev]` in Cargo.toml: the dev
# profile is already `opt-level = 2` with debug info off, so a release tree
# would be a second copy of a large target directory for very little.
PROFILE_DIR=debug
BUNDLE_ID=dev.gpuimaps.iosdemo
APP=target/gpui-map.app

cargo build --target "$TARGET"

rm -rf "$APP"
mkdir -p "$APP"
cp "target/$TARGET/$PROFILE_DIR/gpui-map-demo" "$APP/gpui-map-demo"
cp Info.plist "$APP/Info.plist"

DEVICE="${SIMULATOR_DEVICE:-booted}"
xcrun simctl install "$DEVICE" "$APP"
echo "installed $BUNDLE_ID -> $APP"

if [ "${1:-}" = "--no-launch" ]; then
    exit 0
fi
# `--console` streams the app's stdout/stderr into this terminal, which is
# where every `[map]` line in `app.rs` lands.
exec xcrun simctl launch --console "$DEVICE" "$BUNDLE_ID"

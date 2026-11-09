#!/usr/bin/env bash
# Fails when the library no longer compiles, or its tests fail, on Linux.
# Run it from anywhere
# (on Linux, or in a CI image with the packages listed below):
#
#     ./scripts/check-linux.sh
#
# # Why this script exists
#
# Development happens on macOS, and everything that reaches the screen is
# `#[cfg(target_os = "macos")]` (`render::MapLayer`, a `CAMetalLayer`). A
# change that touches the GPU state without a matching non-macOS arm compiles
# on every developer machine and breaks every Linux/Windows host application,
# with nothing on the Mac to notice.
# Off macOS the map draws offscreen and hands gpui the pixels
# (`offscreen_presenter`); this keeps that path, and the rest of the crate,
# building and tested there.
#
# # The two feature sets
#
# - the crate's defaults, as its own examples build it;
# - `--no-default-features --features probe,sprite_http_fetch`, the lean set
#   a host application that brings its own UI builds with.
#
# # The tests
#
# `cargo test --lib` with the default features. The GPU tests need a Vulkan
# device -- in CI that is Mesa's software one, lavapipe
# (`mesa-vulkan-drivers`) -- and the label tests need fonts (`fonts-dejavu-core`,
# `fonts-noto-core`). Tests whose expectations only hold on macOS (golden
# hashes blessed on Metal, macOS font metrics, a hardware-GPU time budget)
# are `#[cfg_attr(not(target_os = "macos"), ignore = "<why>")]`: `cargo test`
# prints the why next to each. To run them anyway, name them (or their
# module) with `-- --ignored`: a bare `--ignored` also runs the tests that are
# ignored everywhere.
# The sessions open many files at once, more than the usual soft limit.
#
# `MALLOC_ARENA_MAX=2`: the tests run in parallel and push tiles, frames and
# meshes through glibc's per-thread arenas (up to 8 per core of the HOST, not
# of the container), which keep what is freed. Measured on 16 cores: a
# 10.4 GB peak without the cap, 3.1 GB with it, for 5-7% more time; on 4
# cores 5.5 GB against 2.8 GB. Not a leak -- the cap takes two thirds away --
# but enough to exhaust a CI step's memory on a many-core host.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "check-linux: default features"
cargo check --locked --lib

echo "check-linux: lean features (probe, sprite_http_fetch)"
cargo check --locked --lib --no-default-features --features probe,sprite_http_fetch

echo "check-linux: tests"
ulimit -n 65536
export MALLOC_ARENA_MAX=2
cargo test --locked --lib

echo "check-linux: ok"

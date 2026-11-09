#!/usr/bin/env bash
# Fails when the iOS build is broken. Run it from anywhere:
#
#     ./scripts/check-ios.sh
#
# # Why this script exists at all
#
# `ios/` declares its own `[workspace]` (see the comment at the top of
# `ios/Cargo.toml`) so that its iOS-only dependency set -- ureq, winit's iOS
# backend, a Metal-only wgpu -- never reaches the shipped crate's lockfile.
# The cost of that detachment is that NO command run from the repository
# root has ever compiled it: `cargo check -p swat-maps-ios` from here
# answers
#
#     error: package ID specification `swat-maps-ios` did not match any packages
#
# so `cargo check`, `cargo test` and `cargo clippy` have all been green
# while the iOS build carried ELEVEN errors, measured at `29453cd`, tagged
# v0.6.2. All eleven were one cause: the engine's `render/mod.rs` had gained
# `vec_block_bytes`, `heap_block_bytes` and `hashmap_table_bytes`, and the
# hand-maintained `ios/src/render/mod.rs` had not. `web/` had the same three
# names missing from its own restatement on the same day.
#
# This is the one command from the root that notices.
#
# # Why a full `cargo check` and not the native test
#
# `tests/ios_shell_stays_in_step.rs` runs in the shipped crate's own `cargo
# test` and catches the cheap half -- the engine growing a `render` file the
# iOS shell has not decided about, and a `#[path]` pointing at a file that
# has been renamed away. It cannot catch a changed function SIGNATURE or a
# `cfg` expression the iOS target fails; nothing but a compiler can. Two of
# the seventeen errors the browser build carried at the same commit were
# exactly that (`IconPipeline::render` renamed, `collect_candidates` given a
# seventh argument). Both halves are wanted; this is the expensive, complete
# one.
#
# # `-D warnings`
#
# The iOS build is at zero warnings and stays there. The three
# `#[allow(dead_code)]` attributes in `ios/src/render/mod.rs` account for
# all seventeen the `#[path]`-included engine files produce, and are scoped
# to exactly those three modules -- the shell's own files get the lint as
# normal. A build kept at zero can promote the next warning to an error.
#
# Note that this does NOT cap the lints of registry dependencies: cargo
# passes `--cap-lints allow` to everything it did not find by path, so
# `-D warnings` reaches this crate's own source and the `#[path]`-included
# engine source, which is what it is for.
#
# # Why the lib target only, and not `--all-targets`
#
# The same measured reason `scripts/check-wasm.sh` gives: `--all-targets`
# compiles the `#[cfg(test)]` blocks inside the `#[path]`-included engine
# files, and those reach `pollster`, `mvt-reader`, `wgpu`'s test helpers and
# a device -- dependencies this crate deliberately does not carry and a GPU
# a `cargo check` has no business needing. It also compiles `ios/examples/`
# and `src/main.rs`, which are the demo app rather than the engine. The
# engine's own tests run natively, under `cargo test` from the root, on the
# same source. What this script is for is the artifact the app links, which
# is the lib.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=aarch64-apple-ios

# A check that silently succeeds because the toolchain is absent is worse
# than no check: it would report green on a machine that has never compiled
# a line of this. Exit 2 -- distinct from cargo's own 101, so a caller can
# tell "not checked" from "checked and broken".
if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "check-ios: the $TARGET target is NOT installed -- the iOS build was NOT checked." >&2
  echo "check-ios: this is not a pass. Install it and re-run:" >&2
  echo "check-ios:   rustup target add $TARGET" >&2
  exit 2
fi

echo "check-ios: cargo check --target $TARGET in ios/ (-D warnings)"
cd ios
# `2>&1` because cargo writes diagnostics to stderr; the caller wants them
# either way, and `set -e` propagates the exit status.
if RUSTFLAGS="${RUSTFLAGS:-} -D warnings" cargo check --target "$TARGET" --lib 2>&1; then
  echo "check-ios: OK -- the iOS build compiles clean."
else
  status=$?
  cat >&2 <<'EOF'

check-ios: FAILED -- the iOS build does not compile.

  `ios/` reaches the engine by SOURCE (`#[path]` includes, not a
  dependency), so an engine change lands in it immediately. If the errors
  above are unresolved `crate::render::` names, `ios/src/render/mod.rs`
  forwards four engine modules with globs -- a name it does not have lives
  in a FILE that shim does not include, so the fix is a `#[path]` for that
  file (and `tests/ios_shell_stays_in_step.rs` should have said so first;
  check why it did not). If they are unresolved `crate::` names outside
  `render`, `ios/src/lib.rs` holds the module closure. If they are wrong
  argument counts or missing methods, `ios/src/app.rs` is calling an engine
  function whose signature moved, and it has to move with it.

EOF
  exit "$status"
fi

# No Studio crate may enter the iOS shell's graph: the shell goes to the open
# gpui-maps repo at the split and the Studio crates do not (repo-split plan
# §2.3). The agent chat's process-spawning, socket-binding crate is also
# desktop-only (spec 2026-10-07-agent-chat-design.md §11). One `cargo tree`
# lists every package the iOS lib resolves; a failed run is a failure, never
# an empty -- and so passing -- list.
STUDIO_CRATES="gpui-agentic-acp gpui-map-agent-kit gpui-map-dash-ui gpui-maps-studio studio-mobile"
if ! tree_out="$(cd "$(git rev-parse --show-toplevel)/ios" && cargo tree --offline --target "$TARGET" --prefix none -e normal,build 2>&1)"; then
  echo "check-ios: FAILED -- cargo tree could not run in ios/:" >&2
  echo "$tree_out" | tail -5 >&2
  exit 1
fi
found=""
for crate in $STUDIO_CRATES; do
  if grep -qE "^$crate v" <<<"$tree_out"; then
    found="$found $crate"
  fi
done
if [ -n "$found" ]; then
  echo "check-ios: FAILED -- Studio crate(s) in the iOS dependency graph:$found" >&2
  exit 1
fi
echo "check-ios: OK -- no Studio crate ($STUDIO_CRATES) is in the iOS graph."

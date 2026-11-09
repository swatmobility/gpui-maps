#!/usr/bin/env bash
# Fails when the browser build is broken. Run it from anywhere:
#
#     ./scripts/check-wasm.sh
#
# # Why this script exists at all
#
# `web/` declares its own `[workspace]` (see the comment at the top of
# `web/Cargo.toml`) so that its wasm-only dependency set and its pinned
# `wasm-bindgen` never reach the shipped crate's lockfile. The cost of that
# detachment is that NO command run from the repository root has ever
# compiled it: `cargo check -p swat-maps-web` from here answers
#
#     error: package ID specification `swat-maps-web` did not match any packages
#
# so `cargo check`, `cargo test` and `cargo clippy` have all been green while
# the browser build carried seventeen errors. It broke that way four times
# before this script existed -- the JS SDK spike found 18 errors in it, globe
# Task 2 left 23, globe Task 6 held it at 23, and `29453cd`, tagged v0.6.2,
# shipped with 17. Nobody was careless; there was simply nothing to notice.
#
# This is the one command from the root that notices.
#
# # Why a full `cargo check` and not a list-comparison test
#
# Because two of those seventeen errors were not list drift. The engine had
# changed `IconPipeline::render` to `render_batches` and given
# `symbol_pass::collect_candidates` a seventh argument, and `web/src/app.rs`
# still called the old shapes. No amount of comparing module lists finds a
# changed function signature -- only a compiler does. (The list half of the
# problem is gone regardless: `web/src/lib.rs` now `#[path]`-includes
# `src/render/mod.rs` itself, so there is one list instead of two. This
# script is what covers everything else.)
#
# # `-D warnings`
#
# The browser build is at zero warnings and stays there -- the three
# `#[allow(dead_code)]` attributes in `web/src/lib.rs` account for all 50 the
# `#[path]`-included engine files produce, and are scoped to exactly those
# modules. A build kept at zero can promote the next warning to an error,
# which is what catches a re-export that got renamed rather than removed:
# that arrives as `unused import`, not as `error`, and would otherwise scroll
# past.
#
# # Why the lib target only, and not `--all-targets`
#
# Measured: `--all-targets` adds 50 errors that say nothing about the browser.
# It compiles the `#[cfg(test)]` blocks inside the `#[path]`-included engine
# files, and those reach `pollster`, `mvt-reader`, `wgpu`'s test helpers and a
# device -- dependencies the browser crate deliberately does not carry and a
# GPU a `cargo check` has no business needing. The engine's own tests run
# natively, under `cargo test` from the root, on the same source. What this
# script is for is the artifact the browser actually loads, which is the lib.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=wasm32-unknown-unknown

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "check-wasm: the $TARGET target is not installed." >&2
  echo "check-wasm:   rustup target add $TARGET" >&2
  exit 2
fi

echo "check-wasm: cargo check --target $TARGET in web/ (-D warnings)"
cd web
# `2>&1` because cargo writes diagnostics to stderr; the caller wants them
# either way, and `set -e` propagates the exit status.
if RUSTFLAGS="${RUSTFLAGS:-} -D warnings" cargo check --target "$TARGET" --lib 2>&1; then
  echo "check-wasm: OK -- the browser build compiles clean."
else
  status=$?
  cat >&2 <<'EOF'

check-wasm: FAILED -- the browser build does not compile.

  `web/` reaches the engine by SOURCE (`#[path]` includes, not a dependency),
  so an engine change lands in it immediately. If the errors above are
  unresolved `crate::render::` names, `src/render/mod.rs` is the list they
  come from and `web/src/lib.rs` includes it whole -- check that whatever was
  added is not behind a `cfg` the wasm target fails. If they are wrong
  argument counts or missing methods, `web/src/app.rs` is calling an engine
  function whose signature moved, and it has to move with it.

EOF
  exit "$status"
fi

cd - >/dev/null

# The agent chat's process-spawning, socket-binding crate is desktop-only
# (spec 2026-10-07-agent-chat-design.md §11): it must never enter the
# browser graph. `cargo tree -i` fails with "did not match" when absent.
tree_out="$(cd "$(git rev-parse --show-toplevel)/web" && cargo tree --offline --target "$TARGET" -i gpui-agentic-acp 2>&1 || true)"
if ! grep -q "did not match" <<<"$tree_out"; then
  echo "check-wasm: FAILED -- gpui-agentic-acp is in the web dependency graph (or cargo tree could not run); it is desktop-only:" >&2
  echo "$tree_out" | tail -5 >&2
  exit 1
fi
echo "check-wasm: OK -- gpui-agentic-acp is not in the web graph."

# `swat-maps-doc` (the Studio Explorer's document model) is meant to run in
# the browser Studio later, so it is checked for wasm from its first commit.
echo "check-wasm: cargo check --target $TARGET in swat-maps-doc/ (-D warnings)"
if RUSTFLAGS="${RUSTFLAGS:-} -D warnings" cargo check --target "$TARGET" --manifest-path swat-maps-doc/Cargo.toml 2>&1; then
  echo "check-wasm: OK -- swat-maps-doc compiles clean for the browser."
else
  status=$?
  cat >&2 <<'EOT'

check-wasm: FAILED -- swat-maps-doc does not compile for wasm32.

  The crate is pure (serde, serde_json, thiserror; no gpui, no engine). An
  error here means something platform-bound (a file path API, threads, the
  engine) was added to it; that belongs in examples/studio/explorer/.

EOT
  exit "$status"
fi

# `swat-maps-dash` (the dashboard framework's pure crate) runs in the browser
# with the `fetch` transport and no native one (spec §2.2, plan Task 1.1).
# The lib is checked, and the test targets are compiled (`--no-run`) so the
# wasm-target test `a_browser_request_carries_no_auth_header` (C4) builds.
echo "check-wasm: cargo check --target $TARGET in swat-maps-dash/ --no-default-features --features http-web (-D warnings)"
if RUSTFLAGS="${RUSTFLAGS:-} -D warnings" cargo check --target "$TARGET" --manifest-path swat-maps-dash/Cargo.toml --lib --no-default-features --features http-web 2>&1; then
  echo "check-wasm: OK -- swat-maps-dash compiles clean for the browser."
else
  status=$?
  cat >&2 <<'EOT'

check-wasm: FAILED -- swat-maps-dash does not compile for wasm32.

  The crate is pure (serde, unicode-normalization; HTTP through a trait).
  An error here means something platform-bound (threads, the filesystem,
  reqwest outside `http-native`, a database client) reached it.

EOT
  exit "$status"
fi

# `web/serve.py`'s SQL template is pinned against the Rust proxy's fixture
# (`web_proxy_template_fixture`, `test_proxy_template_matches_rust`), but
# nothing else runs the Python half of that pin -- a cargo-only gate stays
# green while `serve.py` builds stale SQL. `python3` is required here, not
# skipped, so this cannot pass silently on a machine that lacks it.
if ! command -v python3 >/dev/null 2>&1; then
  echo "check-wasm: FAILED -- python3 is not on PATH, so web/serve.py --selftest cannot run." >&2
  echo "check-wasm:   install python3, or this gate cannot verify the proxy's SQL template." >&2
  exit 2
fi

echo "check-wasm: python3 web/serve.py --selftest"
if python3 web/serve.py --selftest; then
  echo "check-wasm: OK -- the proxy's SQL template matches the Rust fixture."
else
  status=$?
  echo "check-wasm: FAILED -- web/serve.py --selftest did not pass." >&2
  exit "$status"
fi

# Both spikes onto master `7d2a0c3`

The iOS and WebAssembly spikes reach the engine by `#[path]` module include,
not by `path` dependency. That is the whole design: one unmodified `src/`
tree serving desktop, browser and mobile, with `git diff master -- src/` at
0 bytes on every branch. This note records what it cost to carry that
property across the seven-branch integration that moved master from
`8062381` to `7d2a0c3` (tests 1180 -> 1244).

Short version: **one line per manifest**. Everything else came across
untouched.

## Merge, not rebase

Both spikes already carried a merge commit — an older copy of
`fix-adapter-limits` merged in, then a commit collapsing the branch's own
limits workaround on top (`34792fd` on iOS, `d906e17` on wasm). A rebase
would have had to rewrite that merge, and rewriting a merge is how you lose
the very commit that proves the workaround was deleted deliberately rather
than never written. So: `git merge master`, once per worktree.

It cost nothing. The merge base was `4071831`, which is an ancestor of
master, and **neither spike branch has ever touched a file outside its own
directory** — `ios/*` and `IOS-SPIKE-REPORT.md` on one side, `web/*` and
`docs/superpowers/specs/assets/2026-09-01-wasm-engine-spike/*` on the other.
Master's 32 changed files and the spikes' 20 do not intersect at a single
path. Zero conflicts, both sides.

| branch | merge commit | conflicts |
| --- | --- | --- |
| `spike-ios-engine` | `81141b0` | none |
| `spike-wasm-engine` | `99741ea` | none |

The limits collapse survived intact on both. On wasm it is not merely
present but *exercised*: `?gl` still forces the WebGL2 backend and still
renders (screenshot below), which is the exact path the deleted workaround
used to special-case.

## The one break: `smallvec`

`ZoomValue::stops` on master now stores its stop list in a `SmallVec` —
one stop for a constant, two for a from/to pair, which covers ~85% of every
stop list the crate builds from the vendored styles, so the inline case
never heap-allocates. That put

```rust
use smallvec::{smallvec, SmallVec};
```

at the top of `src/style.rs`, and **`src/style.rs` is in both spikes'
`#[path]` closure**.

A `#[path]` include is a *source* include. It brings no transitive
dependencies with it. The parent `Cargo.toml` declaring `smallvec` does
exactly nothing for a crate that reaches `style.rs` this way — each spike
has to name the dependency itself, in its own `[dependencies]`, or the
module does not compile:

```
error[E0433]: cannot find module or crate `smallvec` in this scope
```

This is the structural tax of the `#[path]` design and it will recur: every
time the engine takes a new direct dependency that is reachable from
`collision.rs`, `geo.rs`, `mvt.rs`, `style.rs`, `style_spec/`,
`symbol_line.rs`, `overlay/geom.rs` or the four `render/` files in the
closure, both spike manifests need the same line. It is cheap, it is
mechanical, and it fails loudly at compile time rather than quietly at
runtime — which is the right trade for what the design buys.

### The version is `"1"`, deliberately

Not `"1.16"`. The engine's own floor was relaxed from `1.16` to `1` because
a consumer's dependency graph carried a `<1.16` ceiling; pinning tighter in
the spikes would reintroduce exactly that hazard for anyone who vendors
this way. `"1"` matches what the engine asks for, and nothing more.

Both lockfiles took **one added line each** — smallvec `1.16.0` was already
resolved in both graphs (transitively, through wgpu and lyon), so this is a
new dependency *edge*, not a new crate and not a version bump:

```
   "serde_json",
+  "smallvec",
   "ureq",
```

## What did *not* break, and why

The brief flagged several master changes as candidate breakages. Each was
checked rather than assumed:

**`TileSource::fetch` -> `Result<Option<Vec<u8>>>`** (from
`feat-absent-tiles`). Does not reach either spike. `tile_source/` is not in
the `#[path]` closure and neither spike implements the trait — iOS fetches
tiles with `ureq` on a worker pool in `ios/src/net.rs`, wasm with the
browser's own `fetch` in `web/src/app.rs`. Both are plain HTTP GETs against
a URL template with no trait in sight, so the signature change has nothing
to land on. Verified, not presumed: both crates compile clean with the new
`tile_source/` on disk.

**Pooled tessellators, the mesh arena, the bounded bake queue, the retained
`CollisionIndex`.** All additive at the API surface. `render/mod.rs` on
master gained two re-exports (`tessellate_plan_with_ranges_cancellable`,
`tessellate_plan_with_ranges_reusing`) and removed none, and each spike's
own `render/mod.rs` — which restates only the subset it needs — was already
naming a strict subset. Both spikes call plain `tessellate_plan(&plan,
4096, COORD_SCALE)`, whose signature is untouched.

**`AllocationStats` gaining three fields and `#[non_exhaustive]`.** Would
have broken any struct-literal construction or exhaustive destructuring.
Neither spike does either — both re-export the type and neither names it
again.

**`GpuContext::max_buffer_size`.** Additive; nothing in either spike
constructs `GpuContext` by literal.

**`tokio`'s new `sync` feature.** Confined to `map_view.rs`'s bake queue,
and `map_view.rs` is not in the closure. Neither spike links tokio at all.

The only residue is warnings: ten per crate, all `dead_code` on engine
items the spikes' narrower module set does not reach
(`note_bake_cancelled`, `BakeInFlight`, `TrackedBuffer::new`,
`project_clip_to_screen`, `OVERLAY_COORD_SCALE` and friends). They are the
expected shape of taking a subset of a crate's modules and were already
present before this merge.

## `src/` stayed clean

```
$ git diff master -- src/ | wc -c
0        # spike-ios-engine
0        # spike-wasm-engine
```

Nothing in the integration required an engine change. That was the bar set
for this work — an engine edit would have been a finding to report, not a
fix to apply — and it was not approached.

## Evidence: iOS

Built for `aarch64-apple-ios-sim`, bundled, installed and launched on the
booted `iPhone 16 Pro` simulator (`0D4619D9-…`).

**A real map renders.** `ios/shots/onto-master-singapore.png` — Singapore at
z13, roads by class, water, parks, land use, the dashed maritime and
administrative boundaries, all in the right colours and the right places.

The console agrees:

```
[map] style: 43079 B, 108 layers compiled in 3 ms
[map] gpu: Metal (Apple iOS simulator GPU) | 1206x2622 px at x3
[map] launch -> first triangle: 414 ms
[map] z 13.00 (tiles z13) @ 1.2895,103.8555 brg 0.0 |
      cover 12: 12 fresh, 0 stale, 0 desc, 0 anc, 0 HOLE |
      16 gpu, 16 decoded, 0 inflight, 0 empty | 6458 frames
```

Twelve of twelve tiles fresh, zero holes, six thousand frames without a
stumble.

### Tests: 734 passed, 0 failed

**Run the test binary from the repo root.** `cargo test` from `ios/` reports
69 failures, every one of them the same panic:

```
panicked at src/../../src/style_spec/mod.rs:10372:79:
run from the repo root: Os { code: 2, kind: NotFound }
```

The vendored-style tests resolve their fixture paths relative to the repo
root, and cargo sets the working directory to the *package* root — which for
this crate is `ios/`, one level down. It is a working-directory artifact,
not a regression, and the tests say so themselves in the expect message.
Run the binary directly with the right cwd and it is clean:

```
$ cd gpui-map-ios   # the checkout
$ ./ios/target/debug/deps/gpui_maps_ios-<hash>
test result: ok. 734 passed; 0 failed; 0 ignored
```

That total includes both interaction fixes this spike is responsible for,
all still green:

- `app::rotation_tests::*` — 16 tests. Two-finger rotation: the deadzone
  that needs a deliberate twist at a usable span, the ground under the
  pinch midpoint staying put, the north detent that snaps and can still be
  turned through, panning that follows the finger on a rotated map.
- `app::pinch_release_tests::*` — 5 tests. The pinch-fling fix: a pinch
  dropped to one finger and released within milliseconds does not fling; a
  post-pinch drag past the threshold does.

(`cargo test --target aarch64-apple-ios-sim` cannot run here at all —
`dyld: DYLD_ROOT_PATH not set for simulator program`. A simulator test
runner is a separate piece of plumbing this spike has never had.)

## Evidence: wasm

Built for `wasm32-unknown-unknown` (release), then the full `web/build.sh`
pipeline through the `wasm-bindgen` CLI:

```
raw cargo output:   5.9M
wasm-bindgen wasm:  4.9M
js glue:            124K
style json:          44K
```

**Ran it in Chrome** — not just compiled it. Served `web/dist/` statically
and loaded both backends:

- `docs/superpowers/specs/assets/2026-09-01-wasm-engine-spike/onto-master-webgpu.jpg`
  — `gpu: BrowserWebGpu`, first triangle at 56 ms, `cover 20: 20 fresh, 0
  stale, 0 desc, 0 anc, 0 HOLE`, 24 tiles decoded.
- `.../onto-master-webgl2.jpg` — `?gl`, `gpu: Gl (ANGLE Metal Renderer,
  Apple M4 Pro)`, first triangle at 94 ms, same 20-fresh-0-HOLE cover.

No console errors or exceptions on either. The WebGL2 run is the one that
matters for the limits collapse: that path used to need a branch of its own
in `app.rs`, `d906e17` deleted it on the strength of the engine's fix, and
master's copy of that fix still carries it.

## One thing worth knowing (not a break)

`cargo test` in `web/` does not compile, and did not before this merge
either. Two structural reasons, both pre-existing:

1. The wasm manifest has no `[dev-dependencies]` at all, so the vendored
   engine modules' own `#[cfg(test)]` blocks cannot find `mvt` (the fixture
   *encoder*) or `pollster`. The iOS manifest declares `mvt` for exactly
   this reason and documents why; the wasm one never did.
2. `web/src/app.rs` calls `wgpu::SurfaceTarget::Canvas`, which only exists
   on a wasm target — so the crate cannot build for the host at all,
   dev-deps or no dev-deps.

The engine's test suite is therefore exercised on the iOS side (734 tests,
same vendored modules, same fixtures) and on master itself (1244), not from
`web/`. Closing this would mean adding the two dev-deps *and* `cfg`-gating
the canvas surface creation — a change to `web/src/app.rs`, which is in
scope for that branch, but it is a separate piece of work and nothing in
this merge made it worse.

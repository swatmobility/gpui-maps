# Collapsing the limits workarounds into the engine

Both sibling shells — `ios/` and `web/` — carried a hand-rolled copy of the
same policy, written outside the engine because the engine did not have it.
`fix-adapter-limits` (`4071831`) put it inside. This document records what the
merge took, what came out of each shell, and what the two platforms actually
reported once the workarounds were gone.

The short version: **`unmet_shader_needs` reported nothing on either
platform.** iOS Metal and WebGL2 both clear every one of the 13 limits this
crate demonstrably consumes, and both render a real map.

---

## 1. What the merge took

`fix-adapter-limits` merged into `spike-ios-engine` and into
`spike-wasm-engine` with **no conflicts** in either worktree — the ort
strategy, no manual resolution, nothing forced. Identical footprint on both
sides:

```
 ADAPTER-LIMITS.md | 248 ++++++++++++++++++++++++++
 src/render/gpu.rs | 525 +++++++++++++++++++++++++++++++++++++++++++++++++++++-
 2 files changed, 770 insertions(+), 3 deletions(-)
```

Nothing in `ios/` or `web/` was touched by the merge itself. That is expected
and worth stating plainly: both shells reach the engine's `gpu.rs` by
`#[path = "../../../src/render/gpu.rs"]` (see `ios/src/render/mod.rs`), so
they compile the shipped source directly rather than a copy of it. The fix
arrives in both shells the moment the merge lands; the only thing left to do
by hand is delete what the shells were doing *instead*.

The three things the merge brings that this work depends on:

- `required_limits(&wgpu::Limits) -> wgpu::Limits` — asks the adapter for its
  own set rather than the WebGPU baseline.
- `unmet_shader_needs(&wgpu::Limits) -> Vec<String>` — the floor that makes
  following the adapter downwards safe. `GpuContext::new` calls it and bails
  *before* `request_device`, naming every shortfall with both numbers.
- `GpuContext` gains `pub max_buffer_size: u64` — the breaking change, and the
  reason both shells had to be edited even where their logic was already fine.

---

## 2. What came out of the iOS shell

`ios/src/app.rs`, in `MapApp::create_renderer`: **60 lines deleted, 10 added.**

Deleted:

- The 28-line `THE ONE ENGINE FINDING OF THIS SPIKE` comment block. It
  described the bug, named the fix, and said explicitly that
  `GpuContext::new` "should derive its `required_limits` from the adapter
  rather than from the WebGPU baseline". That is now what it does, so the
  comment was describing the past.
- `let adapter_limits = adapter.limits();` and the
  `wgpu::Limits::default().check_limits_with_fail_fn(..)` call that
  accumulated a `Vec<String>` of shortfalls with `fatal: false`. This is
  `unmet_shader_needs` in every respect that matters, including the decision
  to report *every* shortfall rather than the first — the engine's version
  makes the same choice, for the same stated reason.
- The `if shortfalls.is_empty() { .. } else { .. }` branch, both arms: the
  `println!("[map] ENGINE FINDING: ..")` reporting, the loop printing each
  shortfall, the hand-rolled `adapter.request_device(&wgpu::DeviceDescriptor {
  required_limits: adapter_limits, .. })`, and the
  `GpuContext { device, queue }` struct literal it fed.

What replaced all of it:

```rust
let ctx = pollster::block_on(GpuContext::new(&adapter))?;
```

The struct literal is gone, so the new `max_buffer_size` field is not a break
here — it is filled in by the constructor, read back off the device, which is
the point.

## 3. What came out of the wasm shell

`web/src/app.rs`, in the startup path: **30 lines deleted, 11 added.**

Deleted:

- The 17-line comment block naming the WebGL2 failure
  (`Limit 'max_compute_workgroups_per_dimension' value 65535 is better than
  allowed 0`) and proposing the fix "take `Limits::downlevel_webgl2_defaults()`
  as the base when the adapter's backend is `Gl`".
- The whole `let ctx = if force_gl { .. } else { .. }` split: the WebGL2 arm's
  `request_device` with `required_limits:
  wgpu::Limits::downlevel_webgl2_defaults()`, and its
  `GpuContext { device, queue }` struct literal.

What replaced it:

```rust
let ctx = GpuContext::new(&adapter)
    .await
    .map_err(|e| JsValue::from_str(&format!("device: {e:#}")))?;
```

Note what did **not** come out. `force_gl` still exists and still selects
`wgpu::Backends::GL` at instance creation — `?gl` remains the way to force the
WebGL2 backend. What is gone is the idea that choosing that backend also
requires choosing a different limits policy. It does not, any more. One
constructor now serves both backends.

The engine's approach is also strictly better than the arm it replaces:
`downlevel_webgl2_defaults()` is a *floor*, and requesting it would have
pinned WebGL2 to that floor even where the browser offered more. Following
the adapter takes whatever ANGLE actually reports.

---

## 4. Did `unmet_shader_needs` report anything?

**No. Nothing, on either platform.**

This is proof by construction, and it is the strongest form available here:
`GpuContext::new` calls `unmet_shader_needs` and `bail!`s if the result is
non-empty, *before* it requests a device. Both shells now have exactly one
device-creation path and it goes through that check. A map on screen means the
check returned an empty vector — there is no route to a rendered frame that
skips it.

### iOS Metal — evidence

`cargo build --target aarch64-apple-ios-sim` finished clean (exit 0; 7
warnings, all pre-existing dead-code notices from engine modules this shell
does not use). Installed and launched on the booted `iPhone 16 Pro`
simulator, `0D4619D9-9E01-4963-8170-7014EDAFE571`:

```
[map] style: 43079 B, 107 layers compiled in 3 ms
[map] gpu: Metal (Apple iOS simulator GPU) | 1206x2622 px at x3 | Bgra8UnormSrgb through an Bgra8UnormSrgb view
[map] surface 1206x2622 px = layer 1206x2622 px (402x874 pt at x3, aspect 0.4600)
[map] launch -> first triangle: 391 ms
[map] z 13.00 (tiles z13) @ 1.2895,103.8555 brg 0.0 | cover 12: 12 fresh, 0 stale, 0 desc, 0 anc, 0 HOLE | 16 gpu, 16 decoded, 0 inflight, 0 empty | 87 frames
```

`0 HOLE`, 12 of 12 tiles fresh, 16 meshes resident on the GPU, and the frame
counter climbing past 1,600 with the coverage line unchanged. No wgpu error,
no uncaptured-error line from the new handler, no `ENGINE FINDING` output
(there is no longer any code that could emit it).

Screenshot: `ios/shots/limits-collapse.png` — the full Singapore basemap at
z13. Roads in their class colours, water, parks, the coastline and the
reservoir, rendered edge to edge. A real map, not a clear colour.

### WebGL2 — evidence

`cargo build --release --target wasm32-unknown-unknown` finished clean (exit
0, same 7 pre-existing warnings), and `web/build.sh` completed the
`wasm-bindgen` step: 4.9 MB wasm, 124 KB of JS glue.

Compiling was the requirement. It also **ran**, which was not: served `dist/`
statically and loaded `?gl` in Chrome, forcing the WebGL2 backend.

```
style: 43079 B fetched, 107 layers compiled in 7 ms
gpu: Gl (ANGLE (Apple, ANGLE Metal Renderer: Apple M4 Pro, Unspecified Version)) | canvas 1492x888 CSS px at dpr 2 -> 2984x1776 (x2) | Rgba8UnormSrgb through an Rgba8UnormSrgb view
page load -> first triangle: 282 ms
```

`gpu: Gl` — the GL backend, through the collapsed constructor, with no
`downlevel_webgl2_defaults()` anywhere in the path. On-screen: the same
Singapore basemap, `cover 20: 20 fresh, 0 stale, 0 desc, 0 anc, 0 HOLE`. No
device error in the console, and the three log lines above are the *only*
console output — nothing matching `error|limit|panic` at all.

### The floor, independently

The engine's own test covers this without a GPU:

```
test render::gpu::tests::every_platform_this_crate_targets_clears_the_shader_needs ... ok
```

It asserts `unmet_shader_needs` is empty for the WebGPU baseline, iOS-shaped
limits, `downlevel_webgl2_defaults()` **and** `downlevel_defaults()`. So
WebGL2 clears the check not merely on this M4 Pro's ANGLE, which reports
generously, but at the spec floor a weaker browser would give.

---

## 5. What this leaves

Neither shell now contains a limits policy. Both call `GpuContext::new(&adapter)`
and nothing else, and the one place that decides what to ask a device for is
the engine — which is where the two spikes independently concluded it
belonged.

`GpuContext::new_headless` still requests `wgpu::Limits::default()` on
purpose, so the test suite keeps running against the portable floor. Nothing
in this work touched that, and nothing in `src/` was modified beyond what the
merge brought in.

# The pinch fling bug: reproduction, diagnosis, fix

User report: "When I make multitouch pinch, sometimes map is activating
acceleration into one direction like inertial mechanism." I.e.: finishing a
pinch sometimes flings the map, as though it had been flicked.

## Reproduction

The diagnosis handed to me was: `on_touch`'s `Ended | Cancelled` arm, on the
FIRST of two fingers lifting, calls `begin_drag` to restart one-finger pan
sampling from the surviving finger -- but that finger is typically still
moving radially, at pinch speed, and its next couple of `Moved` events get
sampled as a pan over a near-zero time window, which `pan_inertia_axis`
divides by and clamps straight to `PAN_INERTIA_MAX_SPEED`.

**I confirmed this by driving `on_touch` directly**, in a throwaway test,
with the exact sequence UIKit delivers for a staggered-lift pinch, using the
REAL clock (`MapApp::now_ms` reads an `Instant`, so a mocked clock would
have laundered away the exact thing under test): two fingers spread apart
over ten `Moved` steps 3ms apart, finger 1 lifts, finger 2 -- still
travelling in the same direction -- is sampled twice more a couple of
milliseconds apart, then lifts too. Console output, before any fix:

```
[touch] Ended id=1 at 100,300 (2 down)
[touch] Moved id=2 at 306,300 (1 down)
[touch] Moved id=2 at 312,300 (1 down)
[touch] Ended id=2 at 315,300 (1 down)
[repro] release: 3 samples, window 5.01 ms, dx=12.00 dy=0.00
[repro] FLING offset=(343.79,0.00) duration=957.5ms
```

3 samples spanning 5ms, from 12px of real (but entirely incidental)
motion, produced a 344px glide over nearly a second -- a full-speed,
essentially-random-direction fling from a gesture the user had already
released. That is exactly the reported symptom.

I also tried to reproduce this on the iOS Simulator using its Option+drag
two-finger gesture, automated via synthetic `CGEvent`s (mouse + Option key)
so the two lifts could be staggered. That surfaced a separate, useful fact
about the harness rather than the bug: **the Simulator ends both synthetic
touches together, at the real mouse-up, regardless of when the Option key
is released** -- releasing Option only freezes the second touch's position,
it does not end it early. So the Simulator's own gesture simulation cannot
produce a genuinely staggered release, and could not be used to exercise
this path. The `on_touch`-level reproduction above is the rigorous
stand-in: it drives the exact code path with full control over the timing
the bug depends on, which the Simulator's input model cannot give.

## What MapLibre does

The engine's existing pan-inertia code is a documented port of MapLibre GL
JS's `handler_inertia.ts`, so I read the equivalent touch handling in
`maplibre-gl-js` (github.com/maplibre/maplibre-gl-js):

- `src/ui/handler/touch_pan.ts` (`TouchPanHandler`) tracks a **centroid**:
  the AVERAGE delta of every touch currently down, computed continuously,
  with no reset on a touch-count change. A symmetric pinch's per-touch
  deltas roughly cancel in that average, so little or nothing gets recorded
  while both fingers are still down.
- `src/ui/handler_inertia.ts` (`HandlerInertia`) never clears its buffer on
  a touch-count change either -- only on the final release, when the last
  active handler goes idle (`handler_manager.ts`'s `_fireEvents`,
  `finishedMoving`).
- Critically, the browser coalesces `touchmove` dispatch to one event per
  animation frame, so MapLibre's shortest possible velocity window is a
  frame wide (~16ms) no matter how fast the underlying touch hardware
  reports.

None of this transfers cleanly to this engine. This engine tracks one
anchor finger for panning, not a centroid (`pinch_state`'s two-finger
midpoint is a separate code path, used only for zoom). And -- this file's
own doc comment on the inertia section already says so -- **UIKit delivers
touch moves as they arrive, not coalesced to a frame**, which is exactly
the platform difference that makes a several-times-shorter, artefactual
velocity window possible here. Porting MapLibre's centroid tracking alone
would not close this gap on iOS, because the frame-coalescing floor that
protects it in a browser has no iOS equivalent.

## What I chose, and why

A **threshold**, not a hard suppression: a one-finger drag segment that
took over from a dropped pinch (`begin_drag(.., from_pinch: true)`) is not
trusted to fling until it has been running on its own for at least
`MIN_POST_PINCH_DRAG_MS` (50ms) at release. A drag segment that started at
a fresh touch-down is never gated.

This was the recommended option, and I agree with the reasoning: a hard
rule ("any gesture that was ever multi-touch never flings") is simpler and
matches how iOS/Google Maps feel, but it kills a real gesture -- pinch,
lift one finger, keep dragging, then flick. The 50ms threshold blocks
exactly the artefact (a velocity window measured in the first instants
after the touch-count changes, when a sample is most likely to be residual
pinch motion) while leaving a genuine continued drag untouched, since any
real drag clears 50ms almost immediately. See `post_pinch_drag_is_trustworthy`'s
doc comment in `ios/src/app.rs` for the full reasoning in place.

Implementation: `MapApp` gained `drag_segment_started_ms: f64` and
`drag_segment_from_pinch: bool`, set by `begin_drag`'s new `from_pinch`
parameter. At release, the gate is applied only when
`drag_segment_from_pinch` is true.

## Tests added

`ios/src/app.rs`, `mod pinch_release_tests` (5 tests):

- Pure-function tests on `post_pinch_drag_is_trustworthy` (the gate itself),
  hand-worked against `MIN_POST_PINCH_DRAG_MS`.
- `a_pinch_dropped_to_one_finger_and_released_within_milliseconds_does_not_fling`
  -- the regression guard: the exact reproduction sequence above, now
  asserting `pan_inertia.is_none()`.
- `pinch_lift_one_finger_keep_dragging_then_flick_still_flings` -- the
  gesture the threshold exists to preserve: pinch, lift one, keep dragging
  well past 50ms, flick. Must still fling.
- `a_fresh_one_finger_flick_that_never_touched_a_pinch_is_not_gated` -- an
  ordinary quick one-finger flick, never near a pinch, is untouched by the
  gate.

All driven through `on_touch` with the real clock, for the same reason the
reproduction was: a mocked clock would not exercise the timing the bug (and
the fix) actually depend on.

Also added: `mvt = "0.15"` under `[dev-dependencies]` in `ios/Cargo.toml`.
This was necessary just to make `cargo test` runnable in this crate at all
-- several of the vendored engine files this crate pulls in by `#[path]`
(`mvt.rs`, `style.rs`, `style_spec/*.rs`, `render/mesh.rs`) build fixture
tiles in their own `#[cfg(test)]` blocks using this encoder crate, which the
parent `gpui-maps` crate already carries in its own `[dev-dependencies]`.
Without it, `cargo test` failed to compile at all (10 `unresolved module`
errors), on any test, unrelated to this fix.

Note: `cargo test --lib` in this crate still reports 68 pre-existing
failures, all in `style::` / `style_spec::` tests that `std::fs::read` a
vendored style fixture (e.g. `examples/liberty-style.json`) by a path
relative to the crate's own manifest directory -- which `cargo test` always
sets as the test binary's working directory, and which is `ios/`, not the
repo root where those files live. This is a structural, pre-existing gap
from vendoring `src/` into a second crate by `#[path]`, unrelated to this
fix, and out of scope here (fixing it would mean touching the vendored
`src/` files, which this spike does not do). `app::` tests are unaffected
and were not part of that failure set before or after this change.

## Verification

`cargo test --lib app::` -- 5/5 pass. `cargo test --lib pinch_release_tests`
-- 5/5 pass, individually and via `cargo test --lib` as a whole (same 68
pre-existing, unrelated failures, no new ones).

`cargo build --target aarch64-apple-ios-sim` and
`cargo build --target aarch64-apple-ios` both build clean.

On the Simulator (`./run.sh`), with a synthetic single-finger drag+release
(no Option key -- a genuine one-finger flick, the case the fix must leave
alone):

```
[touch] Started id=... at 198,270 (0 down)
[touch] Ended id=... at 351,270 (1 down)
```

followed by the periodic status line's `@ lat,lon` changing across several
subsequent frames with no further touch input -- the camera kept moving
after release, i.e. it flung, exactly as it should.

With a synthetic two-finger pinch (Option+drag) that zooms and then
releases (both touches necessarily ending together, per the Simulator
limitation above -- not the staggered case, but a valid check that an
ordinary pinch-and-release is not itself broken): the zoom applied
correctly (`z 13.00` -> `z 13.19` across the gesture) and the release
produced no spurious fling. The staggered-lift case itself -- the actual
bug -- is verified by the `on_touch`-level reproduction and regression test
above, which is the rigorous form the Simulator's input model cannot
provide.

`git diff master -- src/` remains 0 bytes.

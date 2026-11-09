> **Superseded in part by the 3D camera work (2026-09-24,
> `docs/superpowers/specs/2026-09-24-3d-camera-parity-design.md`).** The
> rules below now live in `src/camera_control.rs` and `src/touch_gesture.rs`
> and are shared with native and the browser:
>
> - Bearing is `[-PI, PI)` (`camera_control::wrap_bearing`), not `[0, 2*PI)`.
> - The 7-degree north snap applies ON RELEASE, not live. The live detent
>   described under "The constants" is gone; native's reasoning
>   (`map_view.rs`, `NORTH_SNAP_WINDOW`) is that a snap mid-gesture makes the
>   map fight the fingers. Try it on the device.
> - The twist deadzone and span floor are `touch_gesture::TWIST_DEADZONE_RADIANS`
>   and `MIN_TWIST_SPAN_PX`, unchanged in value.
> - A two-finger vertical drag TILTS (`TwoFingerGesture`, after MapLibre's
>   `TwoFingersTouchPitchHandler`), and a released twist keeps turning
>   (rotation inertia). Both were listed below as not done.
> - `center_after_rotating_about` is `CameraController::rotate_now`, and it is
>   exact under pitch too (the camera's view of the plane is translation
>   equivariant), not the first-order correction described below.

# Two-finger rotation: the constants, the anchor math, and what was tested

The two-finger gesture already carried two of its three components: the
midpoint's motion is a pan, the finger separation's ratio is a zoom
(`update_pinch` in `ios/src/app.rs`). This adds the third. The line through
the two fingers has an ANGLE, and its frame-to-frame delta is a bearing
change -- read from the same one `PinchSample` per frame, applied about the
same one anchor, so a hand can do all three at once without them fighting.

Nothing under `src/` changed: `git diff master -- src/` is still 0 bytes.
The engine already had every piece of the hard part. `Camera::bearing`
exists, and `Camera::eye_target_up` builds the whole rigid frame at
bearing 0 and then rotates it as a unit, which is what keeps `up` exactly
perpendicular to the view direction at combined pitch and bearing. That
math was not touched, reimplemented, or worked around.

## The constants

All three live with the other gesture constants at the top of
`ios/src/app.rs`, each with the full reasoning in its own doc comment. The
short version:

| constant | value | why |
| --- | --- | --- |
| `ROTATION_DEADZONE_RADIANS` | 10 degrees (`PI / 18`) | net twist required before rotation engages |
| `MIN_ROTATION_SPAN_PT` | 40 pt | below this the pair's angle is contact noise |
| `BEARING_SNAP_RADIANS` | 7 degrees | how close to north snaps to exactly north |

**10 degrees**, because at a typical 150 pt two-finger span each finger is
75 pt from the midpoint, so ten degrees is `75 * 0.175 = 13 pt` of
tangential travel per finger -- just past `TAP_SLOP_PT` (12 pt), which is
this shell's own existing answer to "has this finger moved on purpose, or is
this contact-patch noise?". Reusing that number keeps one definition of
intent across the gesture layer instead of inventing a second one. It is
also comfortably short of a quarter turn, so a deliberate twist engages
almost immediately.

It is measured on NET rotation since the gesture's first sample, not on the
sum of per-frame magnitudes: a hand that wobbles while spreading accumulates
a large total and almost no net, and must not rotate. Once engaged it stays
engaged for the rest of the gesture -- a gesture that turns back through its
own start angle has to keep tracking the fingers, not disengage and stutter.
And the deadzone is CONSUMED, not repaid: the sample that crosses the
threshold applies only its own delta, never the ten degrees it took to get
there, which would jolt the map by the whole deadzone at the exact moment
the deadzone exists to smooth.

**MapLibre GL JS does this differently**, and the difference is worth
naming. Its `TwoFingersTouchRotateHandler`
(`src/ui/handler/two_fingers_touch.ts`) sets `ROTATION_THRESHOLD = 25`
PIXELS OF ARC along the circle through the two fingers, so the angular
threshold shrinks as the fingers spread: `50 / span` radians, about 19
degrees at a 150 pt span and 7 at 400. That is a genuine improvement in one
respect -- with the fingers nearly touching, a fixed angle is reachable by
jitter alone. This shell answers that case with `MIN_ROTATION_SPAN_PT`
instead, refusing to read an angle off two fingers that close at all: at a
40 pt span a point of contact-centroid wander is 1.4 degrees of apparent
rotation, at 10 pt it is 5.7 and a resting hand would spin the map. The
fixed angle is simpler to reason about and to test, and it never asks a
wide, deliberate two-hand gesture for a nineteen-degree run-up.

**7 degrees** for the snap is Mapbox/MapLibre's `bearingSnap` default
(`src/ui/map.ts`), taken as-is: there is no reason to invent a different
number for the same job. Small enough that a user aiming near north-east is
never dragged to north; large enough that "put it back the way it was" needs
no precision -- and with no compass button on this spike's screen, the
gesture is the only way back to north, so it has to be forgiving.

It is applied LIVE, as a detent, not once on release. The map holds at
exactly north while the fingers turn through the window and breaks free on
the far side, which is what Apple Maps does. That is why `MapApp` tracks
`pinch_bearing`, the raw unsnapped bearing the fingers have actually asked
for, separately from `camera.bearing = snap_bearing_to_north(pinch_bearing)`.
Feeding the snapped value back into the accumulator would trap the map at
north for the rest of the gesture, however far the fingers turned --
`the_north_detent_can_be_turned_through` is the test that would catch it.
MapLibre snaps once, at the end of the gesture (`handler_manager.ts`'s
`shouldSnapToNorth`), which cannot hold north under a still-moving finger.

## Bearing normalisation (historical)

`normalize_bearing` wraps to **`[0, 2*pi)`** with `rem_euclid`, and
`bearing_from_north` gives the same value signed in `(-pi, pi]` where the
snap window needs it (north has neighbours on both sides of the wrap).

`[0, 2*pi)` rather than `+/-pi` because it reads as a compass bearing, which
is what the value is: 350 degrees, not -10.

The confirmed gap this closes: nothing in `src/` normalises
`Camera::bearing`. It is a plain `f32` fed to `DMat3::from_rotation_z`,
which is happy with any finite value, so a shell that merely accumulates
deltas lets it grow without bound across repeated gestures. That renders
correctly but makes the value useless to log or compare, and it eats f32
mantissa -- at 10,000 rad an f32 step is ~1e-3 rad, so the smallest twist a
hand can express stops resolving. Per the brief, `src/` was left alone; the
normalisation is applied in the shell, on every value the shell writes into
the camera.

`rem_euclid` has one edge: for a tiny negative input it can return the
modulus itself by rounding (`(-1e-9).rem_euclid(TAU)` is `TAU` in f32), so
the function folds that back to 0 and the range really is half-open.
`a_bearing_normalises_into_one_turn_however_far_it_has_wound` checks that,
and checks fifty turns of winding.

## The anchor math

Zoom already had `center_anchored_at`: a zoom is a pure scale about a point,
so the center that keeps the anchor still has a closed form. Rotation does
not. The camera turns about its own look-at point -- the screen centre --
so ground anywhere else slides, and there is no equally tidy formula,
especially not one that stays honest under pitch.

So `center_after_rotating_about` MEASURES the slide instead of deriving it.
Unproject the anchor pixel before the turn and again after it, and translate
the center by the difference:

```
  screen pixel p maps to world w  = C + M(bearing)  * (p - p_center)
  after the turn it maps to    w' = C + M(bearing') * (p - p_center)
  so with C_new = C + (w - w'),  p maps back to w exactly.
```

At pitch 0 -- every bearing this shell can reach, since it has no pitch
gesture -- `M` is a rotation-and-scale, that algebra is exact, and one step
holds the anchor to float precision. Under pitch it degrades to a
first-order correction that would converge if iterated; this does not
iterate, because nothing here can set a pitch. Both unprojections are f64
(`unproject_to_ground_f64`, `world_center_f64`) for the reason `zoom_by`
already spells out: this subtracts two world positions of magnitude ~128-256
to recover a difference of a few pixels over `2^zoom`, and in f32 that
cancellation IS the anchor visibly slipping.

Measured: over a 70-degree twist about a point at (120, 260) on a 390x844 pt
screen -- deliberately nowhere near the centre -- the held ground ends
**0.000031 pt** from where it started.

### Sign convention

Screen y points down, so `touch_pair_angle`'s `atan2` INCREASES for a
visually clockwise turn of the fingers, while the camera's bearing increases
the other way (bearing +90 puts north to the left). `update_pinch` therefore
negates the delta, and `the_map_turns_the_same_way_the_fingers_do` checks it
through the engine's own projection rather than by reading the bearing back
-- a sign error in either place would leave the magnitude right and turn the
map the wrong way.

`signed_angle_delta` brings each frame's difference into `(-pi, pi]`. Taking
the short way round is correct here because a touch frame is milliseconds:
no hand turns more than half a circle between two samples.

## One thing rotation dragged in with it: panning

`pan_by_pixels` predates rotation and read a screen delta straight into
world axes, which the browser shell was free to do because a mouse cannot
turn the map. On a rotated map that sends every drag -- and every fling --
off at the bearing angle to the finger. `screen_delta_to_world` now turns
the delta by the bearing first, the same `from_rotation_z(bearing)` the
camera applies to its own frame.

At bearing 0 the rotation is the exact identity (`cos = 1`, `sin = 0`), so
pan, fling and every pre-existing test are bit-for-bit unchanged until
something actually rotates. `a_screen_delta_is_unchanged_at_north_...`
asserts that as an equality, not an approximation.

## What was tested, and how

`cargo test --lib rotation_tests` -- **16 pass**. `cargo test --lib
pinch_release_tests` -- **5 pass**, unchanged. Whole crate: 634 pass, 68
fail, exactly the same 68 pre-existing `style::`/`style_spec::` fixture-path
failures documented in `PINCH-FLING-FIX.md` (they `std::fs::read` a style
fixture by a path relative to the crate manifest dir, which is `ios/`).
`cargo build` clean for both `aarch64-apple-ios-sim` and
`aarch64-apple-ios`.

### Driven through `on_touch`, because the Simulator cannot reach it

The unit tests drive `on_touch` directly, the same rigorous stand-in the
pinch-fling fix used. Unlike that fix, none of this depends on timing --
rotation is a function of where the fingers are, not when they got there --
so no clock is mocked and no `sleep` appears.

**A new Simulator limitation, found here.** `PINCH-FLING-FIX.md` established
that Option+drag always ends both synthetic touches together. This
investigation found a second constraint: **the Simulator's two synthetic
touches are always mirrored about the DEVICE SCREEN CENTRE**, not about the
point where Option was pressed. Pressing Option at (100, 250) and dragging
still produced touches at (212, 624) and (190, 250) -- midpoint exactly
(201, 437), the centre of a 402x874 pt screen. So the Simulator physically
cannot produce an off-centre pinch midpoint, and cannot exercise the anchor
correction at all: at the screen centre the correction is identically zero,
because the anchor IS the camera's look-at point. It also cannot hold the
span constant while turning, cannot twist by an exact number of degrees, and
cannot place the fingers 30 pt apart.

Everything the Simulator cannot reach is therefore covered at the `on_touch`
level, with the camera's own `unproject_to_ground_f64` /
`project_world_to_screen_f64` used to check where ground actually lands on
screen:

- `the_ground_under_the_pinch_midpoint_stays_there` -- the anchor, at
  (120, 260), far off centre. 0.000031 pt of slip over 70 degrees.
- `the_map_turns_the_same_way_the_fingers_do` -- direction and magnitude,
  measured as a screen-space swing about the anchor, with the distance from
  the anchor preserved to 0.05 pt.
- `fingers_too_close_together_never_rotate` -- 30 pt apart, twisted a
  quarter turn, bearing exactly 0.
- `rotation_stays_engaged_once_the_deadzone_is_cleared` -- twist out 40
  degrees, then turn back 80 through the start angle; every one of those 80
  degrees must land, with no second deadzone and no stall.
- `the_north_detent_can_be_turned_through` -- the raw-vs-snapped bearing
  split.
- `a_rotated_map_pans_where_the_finger_goes` -- a 340 pt drag on a
  45-degree map; the ground under the finger slips **0.000000 pt**.
- `a_twist_that_releases_both_fingers_does_not_fling` -- the pinch-fling
  gate re-checked with rotation in the mix.
- Plus the pure functions: angle, wrap-crossing deltas, normalisation over
  fifty turns, the snap window on both sides of the wrap, and the deadzone.

The expectation for how much rotation a twist should apply is worked from
`ROTATION_DEADZONE_RADIANS` rather than hard-coded, in
`expected_rotation_band`, so changing the constant changes the expectation
instead of silently invalidating the test -- the same discipline the
existing `MIN_POST_PINCH_DRAG_MS` tests follow. Note that winit delivers
each finger's move as its own event, exactly as UIKit hands them over, so
the pair's angle advances in half-steps and engagement lands within one
sample of the threshold: a 40-degree twist in 2-degree steps must apply
between 30 and 32 degrees, and applies 30.

### On the Simulator, from the app's own touch log

Driven with synthetic `CGEvent`s (Option held down for the whole drag, the
pointer swept along an ARC about the pivot). The Simulator's coordinates map
1:1 to logical points here, offset by the window frame.

**A deliberate twist rotates.** 60 degrees of finger, clockwise:

```
[touch] Started id=4687151232 at 321,437 (0 down)
[touch] Started id=4687155600 at 81,437 (1 down)
[touch] rotation engaged after 11.0 deg of twist
[map] z 13.00 ... brg 336.0 ...
[touch] Ended id=4687151232 at 261,541 (2 down)
[touch] Ended id=4687155600 at 141,333 (1 down)
[map] z 13.00 ... brg 310.0 ...
```

60 degrees of twist, 10 to the deadzone, 50 applied: 360 - 50 = 310. The
fingers turned clockwise and the bearing went DOWN, which is the map turning
clockwise with them.

**A straight pinch with incidental rotation does not rotate.** Fingers
spread from 120 pt apart to 340, drifting 6.1 degrees -- under the deadzone:

```
[map] z 12.41 ... brg 21.7 ...
[touch] Started id=4689311472 at 261,437 (0 down)
[touch] Started id=4689311904 at 141,437 (1 down)
[touch] Ended id=4689311472 at 370,455 (2 down)
[touch] Ended id=4689311904 at 32,419 (1 down)
[map] z 13.91 ... brg 21.7 ...
```

The zoom happened (12.41 -> 13.91, one and a half levels). No "rotation
engaged" line. The bearing did not move by so much as a tenth of a degree.

**Rotating near north snaps to it.** From 310, a 57-degree twist back:

```
[map] z 13.00 ... brg 310.0 ...
[touch] rotation engaged after -10.5 deg of twist
[map] z 13.00 ... brg 332.9 ...          <- mid-gesture, tracking the fingers
[map] z 13.00 ... brg 0.0 ...            <- raw bearing 357, snapped to north
```

47 degrees land, which would leave the map at 357 -- 3 degrees short of
north, inside the 7-degree window -- so it holds at exactly 0.0.

**Pan, zoom, double-tap and fling still behave.**

- Fling, at bearing 0: `[touch] Ended ... at 301,255` then the centre moves
  from `@ 1.2584,103.8177` to `@ 1.2459,103.8121` across the following
  status lines with no further touch input, then settles. It still glides.
- Double tap: `[touch] DOUBLE TAP (142 ms, 0 pt apart) -> zoom in`,
  `z 13.91` -> `z 14.91`, one level, with the centre shifting toward the
  tapped point. (A slower synthetic pair correctly logs "tap, but not a
  double".)
- Pan on a ROTATED map, which is the check the `pan_by_pixels` change
  exists for. At `brg 90.1`, a 200 pt drag to the RIGHT moved the centre
  from `1.2478,103.8109` to `1.2523,103.8109`: due north, +0.0045 degrees
  of latitude, with the longitude unchanged to four decimals. At this
  engine's 512 pt tile size and z14.91 the prediction is
  `200 / (512 * 2^14.91 / 360) = 0.00457` degrees, north, zero longitude --
  which is what happened. Before the fix that drag would have gone due
  east.
- And it draws: at `brg 90.1` the Sentosa coastline runs bottom-left to
  top-right; at `brg 0.0`, same centre and zoom, the same scalloped beaches
  run left to right. A quarter turn, on screen, not just in a number.

A physical-device run was not attempted; `aarch64-apple-ios` builds clean.

## What Apple Maps does that this deliberately does not

- **Haptics.** Apple ticks `UIImpactFeedbackGenerator` as the map lands on
  the north detent, which is most of what makes the detent legible. This
  spike has no haptics layer, and adding one for a single gesture is not
  the shape of this change.
- **The compass button.** Apple Maps grows a compass in the corner the
  moment the bearing leaves north, and tapping it animates back. That is a
  UI surface this spike does not have -- and its absence is exactly why the
  snap window is the full 7 degrees rather than something tighter: the
  gesture is currently the only way home.
- ~~**Rotational inertia.** Let go mid-spin in Apple Maps and the map keeps
  turning and eases out, the way this shell's pan already flings. Rotation
  here stops when the fingers stop. It is a real difference in feel, and it
  is deliberate: the pan inertia in this file is a careful port of
  MapLibre's `handler_inertia.ts` with a hard-won iOS-specific fix on top
  (`PINCH-FLING-FIX.md`), and a bearing fling needs the same care and its
  own velocity window. It belongs in its own change, not smuggled in here.~~
- ~~**Pitch.** Apple's two-finger vertical drag tilts into 3D. The camera
  supports pitch and `center_after_rotating_about` notes what it would take
  to stay exact under it, but no gesture here sets one.~~
- **Rotation lock at low zoom.** Apple restricts rotation when zoomed far
  out (where it is mostly meaningless and the globe takes over). This
  rotates at any zoom the shell allows.
- **Snapping to a heading other than north.** Apple's navigation modes snap
  to course-up. There is no location or route here to have a heading.

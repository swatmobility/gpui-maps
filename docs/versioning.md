# Versioning

## The rule

Cargo semver puts the incompatibility signal in the **minor** slot while
the major is 0: `0.4.0 -> 0.4.1` is compatible, `0.4.0 -> 0.5.0` is not.
A consumer pinning `gpui-maps = "0.4"` gets patches automatically and is
never surprised.

So:

- **Patch (`0.4.x`)** -- bug fixes, performance work, memory work, new
  rendering capability, new public items. Anything a consumer's existing
  code keeps compiling through. **This is the normal release.**
- **Minor (`0.x.0`)** -- a removed or changed public signature, a
  behaviour change a caller could be relying on, a raised MSRV or
  dependency floor. **Rare, and the tag message says what breaks.**
- **Major (`1.0.0`)** -- see below. Not soon.

## What went wrong before 2026-08-30

Six tags in three days, `0.1.0` through `0.4.0`. Reviewed against what
they actually contained:

| tag | contents | should have been |
|---|---|---|
| v0.1.1 | overlay realloc leak fix | patch -- correct |
| v0.2.0 | `set_features_from`, `GeometryInput` -- **additive** | `0.1.2` |
| v0.2.1 | `FeaturePaint` re-exports -- additive | patch -- correct |
| v0.3.0 | lighting model, Standard renders dark -- **additive** | `0.1.3` |
| v0.4.0 | GPU buffer fix, counters, registry -- **additive** | `0.1.4` |

None of the three minor bumps broke anything. The minor slot was spent
as a "this release is significant" counter, which is not what it means
and not what a consumer reads it as. Significance belongs in the tag
message and the changelog; the number is a compatibility contract.

Releases are also cheap here and should stay cheap. A tag per fix is
fine. A tag that lies about compatibility is not.

## 1.0.0

1.0 is a promise that the public API stops moving, and it should not be
made while the engine still has holes a consumer can fall into. Both
halves have to be true. This is a checklist, not a feeling:

### API stability

- [ ] Incremental geometry API (`Upsert`/`Remove`/`SetGeometry`) landed.
      Named consumer: a downstream app's `map_push_wanted`, which re-pushes
      the whole overlay when one vehicle's route changes.
- [ ] Paint-only updates (`set_feature_paint`) proven against a real
      consumer workload rather than a benchmark.
- [ ] No public item deprecated-but-still-present.
- [ ] One release cycle with no signature change at all.

### Rendering completeness

- [ ] `fill-extrusion` -- no 3D buildings today.
- [ ] `circle`, `clip`, `model`, `building` layer types.
- [ ] The five label layers blocked on a zoom-by-feature size ramp
      (`city label`, `settlement-minor-label`,
      `settlement-subdivision-label`, `road label`,
      `path pedestrian label`). This is why place labels are sparse at
      mid zoom and why "Singapore" never appears.
- [ ] `line-center` placement and `text-field` shapes.
- [ ] `feature-state`, `distance-from-center` and `pitch` expressions,
      or a documented decision not to support them.
- [ ] Sprite `params` runtime recolouring, or a documented decision not
      to.

### Memory

- [ ] The budget in `docs/superpowers/plans/2026-08-30-memory-budget.md`
      met and measured: ceiling within 4x the steady state, every
      per-tile cache bounded by the viewport that justifies it.
- [ ] A leak test in CI, not just in a session: heap returns to its
      steady state after a closed camera tour.

### Correctness

- [ ] The fingerprint and colour-digest suite covers every layer type
      claimed above.
- [ ] No known-wrong rendering behaviour carried as a backlog entry.

Until every box is ticked, the crate stays on `0.x` however good it
gets. A 1.0 that has to be followed by a 2.0 six weeks later is worth
less than an honest 0.9.

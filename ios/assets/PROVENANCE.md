# `japan-sprite@2x` — the sheet this spike will draw icons from

`japan-sprite@2x.png` (1022x362 RGBA, 81 KB) and `japan-sprite@2x.json`
(321 entries, 35 KB), copied verbatim on 2026-09-10 from the wasm spike's
`web/sprites/`, where they were built by `build_sprites.py` and where that
file's README carries the full recipe.

Every pixel is either drawn by that script or CC0:

* **Route shields, one-way arrows, the city dot** — drawn by
  `build_sprites.py` with Pillow. `jp-national-route-2`/`-3` (国道, the blue
  rounded inverted triangle), `jp-expressway-2`/`-3` (高速道路, green
  rectangle), `jp-prefectural-road-2`/`-3` (都道府県道, blue hexagon),
  `default-1`…`default-6` (the fallback badge, slate, for the style that
  paints its route number white) and `road_1`…`road_6` (the same silhouette
  in the opposite polarity — a white plate with a slate keyline — for the
  two styles that paint theirs dark), `oneway-small`/`oneway-large`/`arrow`,
  `circle_11_black`.
* **POI icons** — the maki set, all 215 SVGs, CC0 1.0 Universal. The licence
  ships beside them as `LICENSE-MAKI.txt`.

Nothing here was downloaded from `api.mapbox.com` or
`tiles.openfreemap.org`, and nothing is traced from either sheet. That is the
whole point: `examples/mapbox-style.json` asks for
`mapbox://sprites/mapbox/streets-v12`, which needs a token AND redistributes
art that is not ours, and this repository is public.

## Why the manifest carries every name twice

`alcohol-shop` and `alcohol_shop`, `art-gallery` and `art_gallery`, and so on
for the whole maki set. Mapbox styles spell maki names with hyphens and the
Liberty lineage spells them with underscores, and the sheet has to answer to
both because the switcher offers styles from both lineages. Two manifest
entries pointing at one drawing costs 40 bytes of JSON and no pixels.

## What it is measured to be worth

`ios/examples/style_audit.rs`, over twelve real OpenFreeMap tiles (Tokyo,
Osaka and Singapore at z6/z10/z12/z13/z14, plus the z14 tiles re-resolved at
z17). "quads" is the count of symbol features whose resolved `icon-image`
names an entry the sheet actually has — a name that misses draws nothing,
silently.

| style | icon-bearing features | quads, this sheet | quads, `ofm@2x` |
| --- | --- | --- | --- |
| `mapbox-omt` | 14350 | 13781 (96%) | 8177 (57%) |
| `liberty` | 15788 | 15172 (96%) | 14719 (93%) |

One sheet serves both, which is why there is only one here. (It served a
third OMT style at 96% too, before that style left with the demo repository
it belonged to.)
`examples/assets/ofm@2x` cannot: it has no hyphenated maki names and no
`oneway-small`, which alone costs `mapbox-omt` 4480 one-way arrows.

## What it still misses

Same harness, the names these tiles asked for that this sheet does not carry,
worst first: `toilets` (155), `atm` (126), `bollard` (103),
`bicycle_parking` (97), `cycle_barrier` (23), `doctors` (23),
`motorcycle_parking` (10), `toll_booth` (9). About 4% of icon-bearing
features across all three styles. All eight exist in maki or are two Pillow
primitives; adding them is a `build_sprites.py` change in the wasm spike and
a re-copy here.

## Nothing reads these yet

Stated plainly because a vendored asset with no consumer looks like an
oversight. The iOS shell has no symbol pass at all, and cannot get one
without an engine change — see `ios/STYLE-SWITCHER.md`, "What stands between
this and a drawn icon". The `.json` is read today by `style_audit`; the
`.png` is kept beside it because the pair is meaningless apart, and because
the moment that change lands the two of them are one `include_bytes!` each.

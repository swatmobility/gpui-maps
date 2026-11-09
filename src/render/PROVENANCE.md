# `src/render` — where the data and the shaders came from

## `bsc5_stars.bin`

The **Yale Bright Star Catalogue, 5th Revised Edition** (Hoffleit, D. &
Warren, W.H. Jr., 1991), quantised. 45,480 bytes, 9,096 records.

Fetched on 2026-09-18 from VizieR catalogue **V/50**:

    https://cdsarc.cds.unistra.fr/ftp/V/50/catalog.gz

    573,921 bytes, sha256
    3dc44b1e90be8fbe5bcc7656032560f51275f985c7e3f783c9028e1838ec7bed

    src/render/bsc5_stars.bin, sha256
    38b8c0320e3805025db25329292c048c9cba8e3054faa93c9ccbb6328bd83211

`render::space::catalogue` decodes it; `render::space::star_mesh` turns it
into the billboards `star_shader.wgsl` draws. Nothing else in the crate
reads it, and it is `include_bytes!`d rather than fetched, so an offline
build has the sky.

### What was taken, and what was dropped

`catalog.gz` holds **9,110** fixed-column records. From each, three fields:

| field | byte columns (1-indexed, inclusive) | ReadMe name |
| --- | --- | --- |
| right ascension, J2000 | 76-77, 78-79, 80-83 | `RAh`, `RAm`, `RAs` |
| declination, J2000 | 84, 85-86, 87-88, 89-90 | `DE-`, `DEd`, `DEm`, `DEs` |
| Johnson V magnitude | 103-107 | `Vmag` |

**14 records were dropped**, every one of them for having no `Vmag` — the
catalogue includes a handful of entries (novae and objects later shown not
to be stars) that carry a position and no photometry, and a star with no
brightness cannot be drawn. **9,096 were kept.** No magnitude cut was
applied: see `render::space::catalogue` for why a cut at the naked-eye limit
of V = 6.5 would have dropped 692 of them and saved 3,460 bytes.

The positions are **J2000**, not the catalogue's alternative B1900 columns.
That distinction is worth the sentence: B1900 positions are up to about 1.4°
from J2000 ones, and a sky built from the wrong column would look entirely
plausible while putting every constellation about two Moon-widths from where
it belongs.

### The quantisation

Five bytes per record, little-endian, no header:

```
    ra : u16 = round(ra_deg  / 360.0 * 65536.0) mod 65536
    dec: i16 = round(dec_deg /  90.0 * 32767.0)
    mag: i8  = round(vmag * 16.0)
```

Decoded by the exact inverse. Measured over all 9,096 records, the
round-trip moves the worst one by **10.90 arcseconds** and **0.030
magnitudes**. At this crate's 60° vertical field of view, a 768-pixel
viewport is 0.078° per pixel, so 10.90" is **0.039 px**; 0.030 mag is 2.8%
of a star's flux. Both are pinned as assertions in
`render::space::tests::the_shipped_catalogue_puts_five_named_stars_where_they_belong`.

Measured sizes of the three forms considered, for the same 9,096 records:

```
    the parsed CSV, as fetched and reformatted    218,946 bytes
    three f32 per star                            109,152 bytes
    u16 RA + i16 Dec + i8 mag  (shipped)           45,480 bytes
```

What was given up is anything a future consumer might want an arcsecond-
accurate catalogue for. This is a backdrop, not an ephemeris.

### What was verified, and how

The parse was **re-derived independently from `catalog.gz` and diffed
against it**, rather than trusted: 9,110 records in, 14 dropped, 9,096 kept,
and the row-for-row worst disagreement with the file that was quantised is
**0.0200 arcseconds** in position and **0.0000** in magnitude.

Identity was then checked against modern J2000 coordinates for named stars,
by nearest-neighbour search over all 9,096 — so a scaled or byte-swapped
decode could not pass:

```
    HR 2491  alpha CMa  Sirius     ra 101.28708  dec -16.71611  V -1.46
    HR 2326  alpha Car  Canopus    ra  95.98792  dec -52.69583  V -0.72
    HR 5340  alpha Boo  Arcturus   ra 213.91542  dec +19.18250  V -0.04
    HR 7001  alpha Lyr  Vega       ra 279.23458  dec +38.78361  V +0.03
    HR  424  alpha UMi  Polaris    ra  37.95292  dec +89.26417  V +2.02
```

Every one is within **0.5 arcseconds** of its modern position. The
magnitudes are the catalogue's own and differ from modern photometry by up
to 0.04 (Arcturus −0.04 against −0.05, Polaris 2.02 against 1.98), which is
catalogue variance and not a decoding error.

The shipped distribution: magnitudes −1.4375 to 7.9375 after quantisation,
and by integer bin 1 / 2 / 11 / 31 / 120 / 335 / 1,047 / 3,328 / 4,166 / 55
from `floor(V) = −2` up to `floor(V) = 7`.

### Licence

**Not independently verified, and that is the honest position.**

This is factual astronomical data from a scientific catalogue that CDS
distributes publicly and without a registration wall; measurements of star
positions and brightnesses are facts rather than creative expression, and
the catalogue has been redistributed in that spirit for decades. CDS/VizieR
asks that the catalogue be **cited**, which is what the citation at the top
of this file is for:

> Hoffleit, D. & Warren, W.H. Jr. (1991), *The Bright Star Catalogue, 5th
> Revised Edition*. VizieR catalogue V/50, Centre de Données
> astronomiques de Strasbourg.

What this repository has **not** done is obtain or read a licence grant from
CDS or from Yale covering redistribution of a derived, quantised binary
inside a compiled library. No such grant was located; no statement of terms
accompanied the `catalog.gz` that was fetched. Confirm with CDS before
redistributing this file in a shipped product if that matters to you — or
generate a blob of your own: the encoder is the three lines above, and
`render::space::catalogue` is the only thing that reads it, so any catalogue
with right ascension, declination and a visual magnitude will do.

The star field is off by default (`render::space::Space::star_intensity` is
`0.0`), so a consumer that has not made that judgement is not shipping this
data's *output* either way.

---

## `atmosphere_shader.wgsl`

The limb glow's scattering integral. **Two upstreams, one chain**, and the
chain matters because the first link is recorded only inside the second
one's source file.

### Link 1 — MapLibre GL JS

`maplibre-gl-js` (github.com/maplibre/maplibre-gl-js) at commit
`0d4b6f1e7d53beac00a4423ce53e7c16a3353b5f` (2026-09-02):

    src/shaders/glsl/atmosphere.fragment.glsl      172 lines
    src/shaders/glsl/atmosphere.vertex.glsl         11 lines
    src/webgl/program/atmosphere_program.ts          the uniform set
    src/webgl/draw/draw_sky.ts                       the gate and the sun
    src/render/painter.ts:641-644                    where it is drawn

**BSD-3.** `LICENSE.txt` in that checkout opens `Copyright (c) 2023,
MapLibre contributors / All rights reserved. / Redistribution and use in
source and binary forms...`, which was read rather than assumed. This is the
licence the whole globe plan already works under, cited in
`render::globe`, `render::subdivide` and `render::space`.

### Link 2 — wwwtyro/glsl-atmosphere

MapLibre's fragment shader carries this header, verbatim:

> /*
>  * Shader use from https://github.com/wwwtyro/glsl-atmosphere
>  * Made some change to adapt to MapLibre Globe geometry
>  */

So the scattering function itself is not MapLibre's. Verified on 2026-09-18,
by fetching the repository rather than by recognising the name:

    https://raw.githubusercontent.com/wwwtyro/glsl-atmosphere/master/LICENSE
    https://api.github.com/repos/wwwtyro/glsl-atmosphere
    https://raw.githubusercontent.com/wwwtyro/glsl-atmosphere/master/index.glsl

**The Unlicense** — a public-domain dedication. The `LICENSE` file is the
standard Unlicense text ("This is free and unencumbered software released
into the public domain... anyone is free to copy, modify, publish, use,
compile, sell, or distribute this software, either in source code form or as
a compiled binary, for any purpose, commercial or non-commercial"), and
GitHub's own metadata for the repository reports `spdx_id: "Unlicense"`,
`key: "unlicense"`. Both were checked, because a `LICENSE` file and a
repository's declared licence have been known to disagree.

The Unlicense asks for nothing — no attribution, no notice. The citation
above is therefore courtesy, not obligation, and it is here because knowing
where an algorithm came from is worth more than the licence question it
settles.

### What MapLibre changed, checked line by line against `index.glsl`

`index.glsl` was fetched and diffed against MapLibre's copy rather than
described from memory:

| | glsl-atmosphere | MapLibre | ours |
| --- | --- | --- | --- |
| primary ray steps | 16 | 5 | 5 |
| secondary ray steps | 8 | 3 | 3 |
| return type | `vec3` | `vec4`, with an opacity | `vec4` |
| march starts at | `iTime = 0` | `iTime = p.x + step/2` | MapLibre's |
| planet clamp | `p.y = min(p.y, rsi(...).x)` | guarded by `p2.x <= p2.y && p2.x > 0` | MapLibre's |
| exposure | none in `index.glsl` | `1 - exp(-rgb)` | MapLibre's |
| gamma | none | `pow(color, 1/2.2)` on rgb **and** alpha | **dropped** |
| path-length factor | none | none | **ours** |

The two that are genuine corrections rather than tuning are the march's
starting offset — `index.glsl` ignores `p.x`, which is only right for a
camera already inside the shell, and a camera in orbit is not — and the
guarded planet clamp, since `rsi` returns `1e5` on a miss.

### What we changed, and where it is argued

- **the frame**: the unit sphere `render::globe` already draws in, rather
  than MapLibre's reconstruction of a camera position through an inverse
  projection matrix (`draw_sky.ts:97-111`);
- **the sun**: `render::atmosphere::Sun`, because this crate has no `light`;
- **the gamma**: dropped, with the measured table in
  `atmosphere_shader.wgsl`'s `fs_main`;
- **the exponent clamp**: `exp(min(-h/scale, 80))`, so a secondary ray
  sampled inside the planet cannot produce an infinity on a driver that
  handles one differently from Metal.
- **the path-length factor**: `atmosphere_shader.wgsl`'s `path_fraction`,
  the ray's own chord through the shell over the longest chord there is.
  **Neither upstream has this**, and it is derived rather than ported: the
  path length comes out of `p.y - p.x`, which is glsl-atmosphere's own `rsi`
  result after MapLibre's two clamps, and the rest is the geometry of a
  100 km shell on a 6,371 km planet — a 22.6x ratio of air between a ray
  aimed at the middle of the disc and one grazing the limb, which the
  exposure and the opacity term between them compressed into about 11x.
  Measured at 768x768 z3 against a control with the pass withheld: the disc
  interior's mean move falls from **13.63 byte units to 0.98** while the rim
  keeps **85%** of itself (d93 to d79). Argued in `fs_main`, graded by
  `the_haze_in_front_of_the_planet_is_a_rim_and_not_a_wash`.

  **Nothing was read from any Mapbox GL JS tree for it.** The observation
  that another renderer's globe has a clean disc interior came from the
  user's own screenshots — output, not source — and is the only input from
  that direction.

### What could NOT be verified

- **MapLibre's own defaults for the light and the sky.**
  `sky.atmosphere-blend` and `light.position` / `light.anchor` have their
  defaults in the `@maplibre/maplibre-gl-style-spec` package, which is not
  present in this checkout (`node_modules/@maplibre` does not exist) — the
  same gap the globe plan records at Task 1 for `transitionState`'s zoom
  expression. So **no reference default is quoted anywhere in this crate**:
  `Atmosphere::intensity` is `1.0` and `Sun::default()` is tilt 55, bearing
  -45 because those are our choices, argued in their own doc comments.
- **Whether `index.glsl` is the exact revision MapLibre took.** The header
  names the repository and not a commit; `master` was fetched on the date
  above and its structure matches MapLibre's line for line, but the
  repository's history was not walked to find the revision current when the
  port was made. Nothing turns on it — the Unlicense is a dedication in
  perpetuity, not a per-revision grant — and it is recorded because "the
  file matches today" is a weaker statement than "the file is the one they
  used".

## `sky_shader.wgsl` and `render::sky`

Ported from **MapLibre GL JS** (BSD-3-Clause, Copyright (c) 2023 MapLibre
contributors):

- the `sky` properties and their defaults: `maplibre-style-spec`
  `src/reference/v8.json`, root `sky` (`sky-color` `#88C6FC`,
  `horizon-color` / `fog-color` `#ffffff`, `fog-ground-blend` 0.5,
  `horizon-fog-blend` / `sky-horizon-blend` / `atmosphere-blend` 0.8);
- the gradient: `src/shaders/glsl/sky.fragment.glsl`
  (`mix(sky, horizon, pow(1 - blend / sky_horizon_blend, 2))`, then
  `mix(colour, transparent, u_sky_blend)`);
- the fade: `src/webgl/program/sky_program.ts` (`u_sky_blend =
  projectionTransition`), and the draw order in `src/render/painter.ts`
  (sky first, atmosphere last);
- `atmosphere-blend` scaling the scattering pass: `src/webgl/draw/draw_sky.ts`.

Ours, each an intentional departure:

- the horizon is the globe's limb, so "distance above the horizon" is an
  angle above the limb's tangent cone rather than a pixel distance; rays that
  hit the sphere are not sky and get the GROUND -- the style background,
  opaque, shaded by the ground's night floor at the hit point -- because a
  frame whose sky draws clears to space and from z6 up nothing else paints
  bare land under the tiles (the pass is behind the ground);
- as the sky fades toward space its horizon colour runs toward its sky colour;
- the pass is gated on GEOMETRY (`sky::sees_above_horizon`: does any corner
  ray clear the limb), not on the globe band, because the projection is
  spherical up to z11 and a frame tilted near the limit at z6-z11 sees a
  curved horizon; whenever it draws the frame clears to the space colour;
- **night**: the reference has none. The sky turns to night through the
  ground's own terminator (`atmosphere::terminator_lit`) and on the night
  side paints nothing, so what shows is space and the stars
  (`sky::sky_night_floor` is `night_brightness^4`, ~0 by default, 1 with the
  terminator off), with a faint twilight band along the limb just past the
  terminator (`TWILIGHT_REACH_SIN` 0.157, `TWILIGHT_STRENGTH` 0.15);
- the pass's alpha is its opacity to the stars (`sky::star_occlusion`), not
  its coverage, so the day sky hides them and the night sky does not;
- a style with no `sky` gets the spec defaults rather than MapLibre's
  transparent "no sky" -- except `horizon-color`, which is our pale blue
  `#C8E0F8` (`style::HORIZON_COLOR_OURS`) rather than the spec's `#FFFFFF`:
  white blended into `#88C6FC` reads as grey haze at the limb (z5.5, day:
  `#E3E8EC`, 4% saturation). The gradient's shape is unchanged. A style that
  declares a `sky` gets the spec's white for anything it leaves out.

Nothing here comes from any Mapbox GL JS 2.x/3.x tree.

## `render::solar` — where the sun is, for an instant

Not a port. **The *Astronomical Almanac*'s low-precision formulae for the
Sun** (U.S. Naval Observatory / H.M. Nautical Almanac Office), the short
series its Section C carries for computer use — six published coefficients,
transcribed into `subsolar_point` and cited in that module's docs.

**Accuracy as stated by the source: about 0°.01 (36 arcseconds) in ecliptic
longitude, for dates between 1950 and 2050.** No coefficient here was
invented and none was tuned.

### What was checked, and against what

- **The two solstices and the two equinoxes**, at their published 2024 and
  2026 instants, against declinations that are definitions rather than
  outputs: ±23.44° (the obliquity of the ecliptic, IAU 2006 giving 23.4393°
  at J2000) and 0°. Measured +23.4354, −23.4354, +0.0015, −0.0012 —
  `the_solstices_and_equinoxes_have_the_declinations_they_are_defined_by`,
  tolerance 0.02°.
- **The equation of time's yearly extremes**, against the published
  analemma's +16.4 minutes in early November and −14.2 around 11 February,
  and against **NOAA's Solar Calculator** for latitude 0, longitude 0 in
  2024, which puts apparent noon at 11:43:31 UTC on 3 November and 12:14:14
  UTC on 11 February. Measured +16.4453 and −14.2014 minutes, and a subsolar
  longitude of 0 at each of four NOAA apparent-noon instants to within
  **0.0535°**.
- **Against a different series.** Meeus, *Astronomical Algorithms*,
  chapter 25 ("Solar Coordinates") with chapter 28's equation of time —
  quadratic in the Julian century where the Almanac's is linear in the day,
  with a third-order equation of centre, the longitude of the ascending node
  and the nutation in obliquity. Written out inside
  `the_low_precision_formulae_agree_with_a_higher_order_series` rather than
  in the module, because its only job is to disagree. Over 146,100 samples
  at 6-hour spacing across 1950-2050 the worst disagreement is **0.003619°**
  of declination and **0.003120°** of equation of time — a third of the
  0.01° the Almanac claims.

### What could NOT be verified

- **The Almanac page number.** The formulae are quoted in this form by many
  secondary sources (Wikipedia's "Position of the Sun" among them, which is
  where they were read and which attributes them to the *Astronomical
  Almanac*); the printed volume was not consulted, so no page citation is
  given. What IS verified is that the coefficients reproduce the four
  solstice and equinox declinations, NOAA's apparent-noon table and Meeus's
  independent series to the figures above — which is a stronger check on the
  transcription than a page number would be.
- **The residual against NOAA's table.** 0.0535° of longitude, 12.8 s of
  time, at the September equinox. It is **not** this formula's error:
  Meeus's series disagrees with the same NOAA figures by the same 12.4 s, so
  what is being measured is NOAA's own whole-second solar-noon iteration.
  0.0535° is 0.33 px of terminator position on a 700 px globe. Recorded
  rather than explained away.

## `extrusion_shader.wgsl` and `render::extrusion::lit_color` — building light

The `fill-extrusion` lighting: ambient plus one directional light, per
vertex. Read in `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js) at commit
`0d4b6f1e7d53beac00a4423ce53e7c16a3353b5f` (2026-09-02):

    src/shaders/glsl/fill_extrusion.vertex.glsl   100 lines, the lighting
    src/webgl/program/fill_extrusion_program.ts   :80-88, the light uniform
    src/util/util.ts:674                          sphericalToCartesian
    src/util/util.ts:69                           translatePosition
    src/webgl/draw/draw_fill_extrusion.ts:20-22   opacity 0 returns early

**BSD-3** (`LICENSE.txt`, `Copyright (c) 2023, MapLibre contributors`), the
licence the globe plan already works under. **Re-expressed in WGSL**, not
copied: the luminance weights, the 0.03 ambient, the `directional` remap, the
vertical gradient's `(t + base) * pow(height / 150, 0.5)` clamped to
`mix(0.7, 0.98, 1 - intensity)`, and the light-colour clamp floor
`mix(0, 0.3, 1 - light_color)` are MapLibre's formulae, line for line.
`render::extrusion::lit_color` is the same arithmetic on the CPU, for tests.

### What we changed

- **R11b: a wall is `nz == 0`, not MapLibre's `normal.y != 0.0`.** MapLibre's
  test is a proxy for "is a wall" that also misclassifies walls facing
  exactly east or west (`ny == 0`), which then get no vertical gradient.
  Harmless in MapLibre's reference images, wrong in a grid city where half
  the walls face east or west. One line (`abs(n.z) < 0.5`); spec §4.4.
- **The viewport rotation** is MapLibre's `mat3.fromRotation(lightMat,
  transform.bearingInRadians)`: `(c·x − s·y, s·x + c·y)` by the compass
  bearing, which is what `Camera::bearing` holds.
- **Alpha is `fill-extrusion-opacity` only**; the colour's own alpha is
  ignored, as in MapLibre. Straight alpha `(rgb, opacity)` with
  `ALPHA_BLENDING` is equivalent to MapLibre's premultiplied
  `v_color *= u_opacity` under `ONE, ONE_MINUS_SRC_ALPHA`.
- **The horizon fade is ours.** After lighting, `rgb` is mixed toward
  `camera.fog_color` by `smoothstep(fog_range.x, fog_range.y, clip.w)`
  whenever `camera.fog_range.y > 0` -- the same fade `shader.wgsl` gives
  every fill and stroke, so a building fades with the ground under it.
  MapLibre's `fill_extrusion.vertex.glsl` has no such step.
- **Globe lighting is not ported**: extrusions draw on flat frames only (R1).
- **`fill-extrusion-translate`** is applied as a world-space offset,
  `translate_px / 2^zoom`, pushed through `camera.view_proj` the way
  `shader.wgsl` pushes a stroke corner, rather than MapLibre's
  `a_pos + u_fill_translate` in tile units: `model.tile_units_per_px` is
  0.0 on every flat frame (`GlobeParams::FLAT`), and the world offset needs
  no per-tile scale. The offset itself is MapLibre's: a `viewport` anchor
  is turned by `+bearing`, `(x·cos b − y·sin b, x·sin b + y·cos b)`, as
  `translatePosition` does for a tile-unit caller (`util.ts:69`,
  `inViewportPixelUnitsUnits = false`); a `map` anchor is not turned
  (`render::extrusion::frame_translate`). Only a literal `[x, y]` compiles;
  an expression degrades to `[0, 0]` with a notice (spec deviation 4).
- **`sphericalToCartesian`** (`util.ts:674`) is ported as
  `render::extrusion::light_direction`: `azimuth + 90°`, then
  `r·(cos az·sin polar, sin az·sin polar, cos polar)`, the raw `r`-length
  vector, rotated by `+bearing` for a `viewport` anchor. In our tile frame
  (x east, y south) the default `[1.15, 210, 30]` points up and to the
  north: the north wall is the lit one (the Task 9 GPU test,
  `the_wall_facing_the_light_is_brighter`: 360 against 240).
- **Opacity 0 skips the layer entirely**, prepass included, as MapLibre's
  `if (opacity === 0) return;` does (`frame_plan::extrusion_plan`).
- **The vertical gradient is per vertex and clamped**, as in MapLibre, so
  at the default intensity 0.5 its floor is 0.84 and a wall under
  150 · 0.84² ≈ 106 m gets 0.84 at both foot and head: a flat darkening,
  no ramp. Taller walls ramp. That is for a BASE-0 feature, and the
  threshold is on the feature's roof height, not its wall length: the term
  is `(top + base) · sqrt(height / 150)` with `base` in metres added to the
  0/1 `top` flag, so a part raised above about 1-2 m (`render_min_height`
  on a building:part) reaches the clamp's 1 at its foot and gets no
  gradient and no darkening at all. Not a divergence -- recorded because it
  surprises (Task 9's GPU test proves the ramp on a 200 m wall;
  `a_raised_part_gets_no_vertical_gradient` pins the raised case).

## `extrusion_gpu` translucent passes — `fill-extrusion-opacity` < 1

The two-pass translucent draw. Read in `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js) at
the same commit as the lighting above (BSD-3):

    src/webgl/draw/draw_fill_extrusion.ts:27-47   opaque vs two-pass branch
    src/render/painter.ts:395-405                 stencilModeFor3D

**Re-expressed**, not copied. A layer whose opacity is below 1 draws every
tile twice, layer-major: first depth only (`ColorMode.disabled`, depth
`LEQUAL` read-write, stencil off) so the depth buffer holds the layer's
nearest surface; then colour, depth `LEQUAL`, with the stencil
`NOTEQUAL ref` / `KEEP, KEEP, REPLACE`, masks `0xFF`, so a pixel blends
once per layer even where two surfaces sit at the same depth. Our
`ExtrusionPipeline::{prepass, translucent}` and `frame_plan::extrusion_plan`.

### What we changed

- **The colour pass does not write depth** (MapLibre's is `ReadWrite`).
  The prepass already wrote exactly these depths, so the write changes
  nothing a later draw can see; leaving it off states that.
- **The stencil reference is the layer's index, `index % 255 + 1`,** and the
  stencil is cleared to 0 once, when the tile pass opens. MapLibre hands
  out `nextStencilID++` and clears when it passes 255; our plan is decided
  per frame, so the index is the same thing without a counter. A layer
  past the 255th draws opaque (debug-asserted); no style has more than 2.
- **Straight alpha**, as for the opaque pipeline above: `(rgb, opacity)`
  under `ALPHA_BLENDING` equals MapLibre's premultiplied colour under
  `ONE, ONE_MINUS_SRC_ALPHA`, and it is what every tile fill here uses.

## Line progress, `line_fx` and the trim — `render::mesh`, `shader.wgsl`

**MapLibre GL JS (BSD-3-Clause), followed for meaning, not copied.** Read
from the sparse clone at `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js) (commit `0d4b6f1e`):

- `src/data/bucket/line_bucket.ts` — `lineClips`, `lineSoFar` and
  `totalDistance`: with `lineMetrics`, each vertex carries its distance along
  the feature's line divided by the line's total length, 0 at the first
  point and 1 at the last. `render::mesh::append_stroke` computes the same
  ratio per LineString (`write_line_progress`: `line_distance / total`, the
  total being the `distance_so_far` its runs and apex windows accumulated),
  as an `f32` vertex attribute (`Vertex::line_progress`, location 11) rather
  than MapLibre's 15-bit packing into `a_data`. Its "reset `distance` past
  `MAX_LINE_DISTANCE / 2`" wrap is not needed here: `line_distance` is an
  `f32` already, and a metrics line is one whole-source mesh (plan D3).
- `src/shaders/glsl/line_gradient.vertex.glsl` and
  `line_gradient.fragment.glsl` — what the progress is FOR: `v_uv.x`
  (`a_uv_x`, the line progress) interpolated across the fragment. Here it is
  `VertexOutput::line_progress`, and `fs_main` discards outside
  `model.line_fx`'s `[x, y]` when `w` is on — the trim. MapLibre has no
  trim uniform of its own. `[x, y]` is the VISIBLE range of progress: the
  COMPLEMENT of Mapbox's `line-trim-offset`, which names the hidden range
  (MapLibre's spec has no `line-trim-offset`; no Mapbox GL JS source was
  read for this). A caller holding a `line-trim-offset` `[0, b]` passes the
  kept range `[b, 1]`; `MapView::set_source_line_trim` takes the kept range. `line_fx.z` (opacity) is a plain per-draw multiply in the
  vertex stage, which has no reference.

Every value a vertex without metrics carries (`-1.0`) and every default
(`LINE_FX_NONE = (0, 1, 1, 0)`) is chosen so an existing frame is
bit-identical. No Mapbox GL JS 2.x/3.x source was opened for any of this.

## `overlay::tile_index` — tiling a runtime GeoJSON source

Not in `src/render`, but recorded here with the crate's other ports.

**geojson-vt 4.0.2 `simplify.js`, `clip.js`, `index.js`, `tile.js`, ISC,
Copyright (c) 2015, Mapbox.** Read from the npm package's `src/`
(`package.json` version 4.0.2). Ported, not wrapped: `tile_index/simplify.rs`
is `simplify.js` made iterative, `tile_index/clip.rs` is `clip.js`'s
`clipLine` split into a ring and a line form, and `tile_index/mod.rs` follows
`index.js` (`splitTile`'s eager pass to `indexMaxZoom` over
`indexMaxPoints`), `tile.js` (`addLine`'s tolerance-pixel ring drop, tolerance
0 at `maxZoom`) and `convert.js` (the simplification floor). Each of the three
files carries the copyright line and the ISC permission notice in its header,
as the licence requires:

> Copyright (c) 2015, Mapbox
>
> Permission to use, copy, modify, and/or distribute this software for any
> purpose with or without fee is hereby granted, provided that the above
> copyright notice and this permission notice appear in all copies.
>
> THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
> WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
> MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
> SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
> WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
> OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
> CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

Deliberate deviations from geojson-vt, for memory: one geometry copy (u32
fixed-point Mercator plus a u8 minimum zoom per vertex) instead of f64
triples plus a clipped copy per index level; index tiles hold feature
references; `cut` filters by minimum zoom before clipping; and a polygon
whose outer ring is dropped drops its holes too. The tile buffer (128 px)
follows MapLibre GL JS (BSD-3) `geojson_source.ts`, cited, not copied.
Nothing here comes from any Mapbox GL JS 2.x/3.x tree.

## `src/overlay/cluster.rs` — Supercluster and KDBush, ported

The point clustering and its KD-tree are a Rust port of **Supercluster 8.0.1**
(`index.js`) and **KDBush 4.0.2** (`index.js`), read from the npm packages.
The algorithm (greedy radius clustering per zoom over a static KD-tree, the
cluster-id encoding, `getChildren`, `getLeaves`, `getClusterExpansionZoom`,
`point_count_abbreviated`) is theirs. Two changes, both documented in the
module: neighbours are visited in ascending index order, and each item
carries a representative leaf for the non-clustered level of detail. The
cluster option names and defaults follow MapLibre GL JS (BSD-3)
`geojson_source.ts`, cited, not copied.

Supercluster:

    ISC License

    Copyright (c) 2021, Mapbox

    Permission to use, copy, modify, and/or distribute this software for any purpose
    with or without fee is hereby granted, provided that the above copyright notice
    and this permission notice appear in all copies.

    THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
    REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND
    FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
    INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS
    OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
    TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF
    THIS SOFTWARE.

KDBush:

    ISC License

    Copyright (c) 2018, Vladimir Agafonkin

    Permission to use, copy, modify, and/or distribute this software for any purpose
    with or without fee is hereby granted, provided that the above copyright notice
    and this permission notice appear in all copies.

    THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH
    REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND
    FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT,
    INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS
    OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
    TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF
    THIS SOFTWARE.

## `["line-progress"]`, `line-gradient` and `lineMetrics` on overlay sources — `style_spec::expression`, `overlay::source_paint`, `overlay::plan`, `render::mesh`

**MapLibre GL JS (BSD-3-Clause), followed for meaning, not copied.** Read
from the sparse clone at `maplibre-gl-js` (github.com/maplibre/maplibre-gl-js) (commit `0d4b6f1e`),
except where noted:

- `["line-progress"]` — defined in MapLibre's style-spec package
  (`@maplibre/maplibre-gl-style-spec`, `src/expression/definitions/index.ts`,
  the `line-progress` entry, which reads the `lineProgress` evaluation
  global; that package is not in the clone and was not read — its meaning
  is taken from how `src/webgl/draw/draw_line.ts` drives it:
  `renderColorRamp({.., evaluationKey: 'lineProgress'})`). Here
  `Scope::with_line_progress` supplies the value; outside a
  `line-gradient` evaluation it is an evaluation error (MapLibre refuses it
  at parse time; story-content review F37 accepts the difference).
- `line-gradient` — `src/webgl/draw/draw_line.ts` (`updateGradientTexture`:
  a texture of width `textureResolution = 256`) and `src/util/color_ramp.ts`
  (`renderColorRamp`: pixel `i` is the expression at progress
  `i / (width - 1)`). `overlay::source_paint::LineGradient` is that table,
  256 straight-sRGB entries at `i / 255`, sampled linearly (`at`) — the
  texture's `LINEAR` filter — for a gradient of any other shape. A top-level
  `["interpolate", ["linear"], ["line-progress"], ..]` is instead evaluated
  EXACTLY through its stop colours, and a `["step", ["line-progress"], ..]`
  as piecewise constant — MapLibre's `NEAREST` texture
  (`bind(stepInterpolant ? NEAREST : LINEAR)`, whose resolution it scales up
  with line length) at unlimited resolution. The fragment shader's
  `texture(u_image, v_uv) * (alpha * opacity)`
  (`src/shaders/glsl/line_gradient.fragment.glsl`) is replaced by PER-VERTEX
  colour: `OverlayPlan::colour_mesh` writes `gradient.at(progress)` into each
  metrics vertex, its alpha times the vertex's own (the resolved
  `line-color` alpha, which carries the feature's opacity — MapLibre ignores
  `line-color` under a gradient but multiplies `line-opacity`), and the
  rasteriser interpolates. The plan cuts every line at each interior stop
  (a step at `p ± eps`, `eps` = max(1/4096 of the line, one grid unit)), so
  a linear gradient is exact and a step's edge spans `2 eps`; then up to 64
  uniform parts per LineString (`render::mesh::densify_at_progress`) for
  the curved shapes, shared out by length under a per-source point budget
  equal to the D3 gate. No texture exists, so no WebGL2 texture limit
  applies.
- `lineMetrics` — `src/source/geojson_source.ts` (the `lineMetrics` source
  option, default `false`) and `src/style/style_layer/line_style_layer.ts`
  (a gradient layer re-renders its ramp when `line-gradient` changes).
  The style spec documents `line-gradient` as requiring a GeoJSON source with
  `lineMetrics` (without it there is no progress to evaluate at);
  `OverlayStore::declare_source_paint` refuses the combination ("line-gradient
  needs lineMetrics"). A `lineMetrics` overlay source is never tiled and is
  refused over the tiling threshold (story-content plan D3), because its
  progress is computed over whole LineStrings.

## `arc_shader.wgsl` and `render::arcs` — origin-destination arcs

deck.gl's ArcLayer (`modules/layers/src/arc-layer/arc-layer-vertex.glsl.ts`
and `arc-layer-fragment.glsl.ts`, **MIT**, Copyright (c) vis.gl
contributors), read from the local clone at `deck.gl` (github.com/visgl/deck.gl) and
re-expressed in WGSL. Taken: the `paraboloid` height with both ends on the
ground (peak = `height / 2` x chord), `getSegmentRatio`'s smoothstep spacing,
the great-circle slerp on the globe with the lift scaled by the central
angle, the screen-space extrusion perpendicular to the strip, the
source-to-target colour mix, and the shortest-way longitude unwrap.

What differs, and why:

- **Segment count per length class**, not one `numSegments` for every arc:
  `ArcSet::new` sorts arcs by chord length into powers-of-two classes and
  each class draws with what its longest arc needs on screen (`segments_for`,
  at most deck's 50). Kepler's 680k commutes are mostly a few km; at its
  view this is 16M vertices instead of 69M.
- **A per-arc cull** on flat frames: the box of the two ends and the same
  ends raised to the peak, all four outside one clip plane, collapses the
  strip.
- **Sub-pixel widths** keep a one-device-pixel strip at full coverage and put
  the width into alpha; from a pixel up, half a device pixel of feather
  each side (deck's `ANTIALIASING` edge, without `fwidth`).
- **The antimeridian**: each flat point is wrapped to the world copy nearest
  the camera and the one segment that crosses the seam is faded out, rather
  than deck's split of the strip at lon 180.
- **Globe occlusion** is the analytic eye-to-point test of
  `render::altitude::closest_approach`, not a depth buffer.

## `trips_shader.wgsl` and `render::trips` — animated trips

deck.gl's TripsLayer (`modules/geo-layers/src/trips-layer/trips-layer.ts`,
**MIT**, Copyright (c) vis.gl contributors), read from the local clone at
`deck.gl` (github.com/visgl/deck.gl) and re-expressed in WGSL. Taken: the semantics and
the shader shape -- a timestamp per path vertex, the time interpolated
along each segment into the fragment, `currentTime`, `trailLength` and
`fadeTrail` as uniforms, and the fragment rule `vTime > currentTime ||
(fadeTrail && vTime < currentTime - trailLength)` discards, else alpha x
`1 - (currentTime - vTime) / trailLength`. Also deck's PathLayer trick of
reading a segment's start and end from ONE vertex attribute bound twice,
one vertex apart (`vertexOffset: 1`).

What differs, and why:

- **No PathLayer joins or caps**: each segment is its own screen-space quad
  (as `arc_shader.wgsl`'s strip, half a device pixel of feather, sub-pixel
  widths into alpha). Kepler's trails are 1-2 px; a mitre would cost a
  third vertex read per segment for nothing visible at that width.
- **A vertex-stage cull**: a segment wholly after the clock, or wholly
  behind a fading trail, collapses before rasterising; deck discards per
  fragment only.
- **Times relative to the set's earliest**, f32 (deck stores them as the
  caller gives them, and leaves the precision to the caller).
- **Globe**: the unit sphere with the arcs' analytic occlusion, and long
  segments cut on the CPU along the great circle (`MAX_SEGMENT_DEG`), so a
  chord neither cuts through the planet nor bends; flat segments take the
  shortest way across the antimeridian.

## `heatmap_shader.wgsl` and `render::heatmap` — screen-space density

deck.gl's HeatmapLayer (`modules/aggregation-layers/src/heatmap-layer/`:
`weights.wgsl.ts`, `max.wgsl.ts`, `triangle-layer.wgsl.ts`; MIT, vis.gl
contributors), read from the local clone at `deck.gl` (github.com/visgl/deck.gl) and
re-expressed, not copied. The three passes are deck's: a Gaussian per
point summed into a float target (deck's kernel constant
`exp(-(2d)^2 / 0.05555) / (1.77245385 * 0.166666)`), a `Max` blend of one
point per texel into a 1x1 target, and a composite that divides by that
maximum, looks the colour up in a ramp and fades below `threshold`.

What we changed:

- The density target is the frame's own half-resolution screen, not
  deck's texture framed to the data's bounds: the map already has a
  screen-space frame and a globe blend (`arc_shader.wgsl`'s), so the
  quad is extruded on screen in both projections.
- The max pass reads every second texel each way (a quarter of deck's
  point count): a Gaussian of at least a few texels cannot hide between
  them.
- `R16Float` rather than deck's `RGBA32F`: it blends without
  `FLOAT32_BLENDABLE`. A texel past a few thousand overlapping kernels
  stops growing, which flattens only the top of the ramp.
- Up to four per-point filter values and windows, as the point sources'
  GPU filter, instead of deck's DataFilterExtension.
- The ramp follows Kepler's `heatmap-color`: transparent at 0 and each
  colour at `(i + 1) / n`.

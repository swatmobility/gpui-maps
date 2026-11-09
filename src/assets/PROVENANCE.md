# `swat-on-light@2x.png` / `swat-on-dark@2x.png`

The SWAT Mobility brand mark -- the symbol and the "SWAT" wordmark set as one
horizontal lockup -- drawn bottom-left on every map this crate renders.

174x36 straight-alpha RGBA PNG each, 3,624 and 3,473 bytes, both
`include_bytes!`-ed into the binary by `src/brand_mark.rs`. That is 2x of the
mark's default drawn size of 87x18 logical px
(`BrandMark::DEFAULT_HEIGHT_PX`).

It is drawn at `BrandMark::DEFAULT_OPACITY`, 0.80, not at full strength --
see that constant, and the section on it below.

`on-light` carries the near-black `#11182B` ink and is for a light basemap;
`on-dark` carries white ink and is for a dark one.

**Each file is one colour and an alpha channel.** Every texel carries the same
rgb; only alpha varies. That is asserted, not assumed --
`brand_mark::tests::the_artwork_is_monochrome_by_construction` -- and it is
also what makes the two files one shape in two inks: their ALPHA channels are
byte-identical, which
`both_vendored_artworks_decode_to_the_same_geometry_with_an_alpha_channel`
checks, so neither can be re-cut without the other.

## The two naming schemes are inverted, and that is the trap

**These files are named for the BACKGROUND. The upstream files are named for
the INK.** They are exact opposites, and wiring them together by matching the
words is how a white logo ends up drawn on a white map.

| upstream (SWAT desktop app) | its ink | the basemap it is for | here |
| --- | --- | --- | --- |
| `mark-dark.svg` + `wordmark-dark.svg` | `#11182B`, dark | LIGHT | `swat-on-light@2x.png` |
| `mark-light.svg` + `wordmark-light.svg` | `white` | DARK | `swat-on-dark@2x.png` |

The lockup doubles the chance of getting this wrong, because there are now
two upstream files per variant and mixing them -- a dark symbol beside a white
wordmark -- would still compose, still draw, and be half invisible. It cannot
happen silently: `the_artwork_is_monochrome_by_construction` would find two
rgb values in one file.

Upstream's own call site is what settles it, not either filename:
`BrandMarks::mark(dark_basemap: bool)` in
its `panels/map_style.rs` reaches for `mark-light.svg` when
`dark_basemap == true`. This repository keeps the background-named
convention, deliberately, because it is the one that cannot be misread at the
point of use -- `BrandMarkVariant::OnDark` says what it is FOR.

The mapping above was verified by reading the SVGs' `fill` attributes and
then, after the cut, by reading the PNGs' own dominant fully-opaque texel.
`brand_mark::tests::each_variant_carries_the_ink_its_name_promises` is that
second check, kept as a test: nothing else in the suite would go red if the
two files were swapped, because a white mark on a white map draws perfectly
well and is merely invisible.

## What it is, and what is not recorded about it

**This is a trademark, not a licensed third-party asset.** It is the mark of
SWAT Mobility, whose product this renderer is, vendored into this repository
deliberately and as part of that product rather than as an example or a
placeholder. It is drawn by default for the same reason Mapbox's mark is: a
map drawn by this engine says which engine drew it.

**No trademark licence or written permission is recorded here, and none was
sought** -- stated plainly rather than left to be assumed, the way
`examples/assets/PROVENANCE.md` states that the sprite sheet's terms were
never confirmed. There was nobody to obtain one from: this is the company's
own mark in the company's own renderer. What follows from that is simply
that nothing about this file appearing in a public repository grants a
reader any right to the mark itself; the source is published, the trademark
is not licensed with it.

Contrast the LICENCE credit in the opposite corner, which is the tile
SOURCE's and not this crate's. It has no public API at all and changes when
the source changes -- its colours follow the map's tone
(`attribution::AttributionTone`) and cannot be set by a host. The mark does not change when the source does.

## Replacing the artwork

If these two files are ever re-cut -- a rebrand, a third density -- the code
imposes only that both variants share one aspect ratio and one pixel size.
`brand_mark::decode` reads whatever PNG is here and `brand_mark::instance`
derives the drawn width from its pixels, so a new pair at the same 4.8333
aspect needs no code change at all.

The DESIGN imposes rather more, and it is all in tests rather than in this
file, so a re-cut that breaks it says so:

- `the_lockup_sits_on_its_own_grid` segments the artwork at its 50% alpha
  boundary and checks the five inked elements (symbol, S, W, A, T), the 6 px
  left margin, the ZERO top and bottom margins, the 18 px symbol, the 12 px
  cap height, the 67% cap-to-symbol ratio, the 6 px gap, and that each of
  those is a whole multiple of `brand_mark::GRID_UNIT_PX`;
- `a_default_mark_stands_in_the_credits_own_band` checks what the lockup
  shares with the licence credit in the opposite corner -- the same 18 px
  band, ink edge on plate edge -- computed from the credit's own constants,
  so it fails if the credit moves as well as if the mark does. It also pins
  what that costs: the wordmark's baseline sits ~1.72 px below the credit's,
  where the previous cut's happened to coincide with it;
- `the_artwork_is_monochrome_by_construction` and the alpha-equality clause
  above cover the ink;
- `the_mark_never_draws_its_ink_into_a_field_of_the_same_ink` (in
  `map_view.rs`) renders the corner over a light clear and a dark one and
  checks the ink moves in the right DIRECTION at each -- the one failure
  none of the above can see, because a mark drawn in the background's own
  colour draws perfectly and is merely invisible.

## How the two files were produced, on 2026-09-20

Sources, all four from `assets/brand/` in SWAT's own
desktop app -- the same files that app draws on its own maps, via
`BrandMarks` in `panels/map_style.rs` (used from `vehicle_map.rs` and
`agent_panel.rs`):

| file | viewBox | ink | what it is |
| --- | --- | --- | --- |
| `mark-dark.svg` | 106 x 116 | `#11182B` | the symbol alone |
| `mark-light.svg` | 106 x 116 | `white` | the symbol alone |
| `wordmark-dark.svg` | 341 x 78 | `#11182B` | "SWAT", four capitals |
| `wordmark-light.svg` | 341 x 78 | `white` | "SWAT", four capitals |

Each is flat paths, no embedded raster, no text elements, no gradients. Within
each pair the two differ in exactly one byte run -- the `fill` -- which is
upstream's own point about them: the artwork is a single-colour mask, so the
dark-ink twin is an exact fill substitution rather than a redraw, and neither
file is a hand-made copy that can drift from the other. **Monochrome was
therefore a property of the sources, and the job here was not to desaturate
anything but to avoid introducing a second colour.** Upstream considered and
rejected the obvious alternative, a dark scrim pill behind one white logo on
every style, on the grounds that it puts a box on the map to solve a problem
the artwork can solve by itself. That reasoning carries over here unchanged.

### The composition

The two PNGs are not crops of anything. A composing SVG sets the symbol and
the wordmark into a `viewBox="0 0 87 18"` -- the mark's drawn size in logical
pixels -- and that is rendered:

    <svg viewBox="0 0 87 18">
      <g transform="translate(6,0) scale(0.155306299)">          <!-- mark-*.svg's path      -->
      <g transform="translate(28.322760,2.5) scale(0.155591572)"> <!-- wordmark-*.svg's paths -->
    </svg>

Both scales and both translations are solved from the sources' measured INK
bounding boxes, not from their viewBoxes. That distinction is the whole reason
the gap comes out right: `mark-*.svg`'s ink stops 0.9 units short of its
viewBox's right edge and 0.1 short of its bottom, and `wordmark-*.svg`'s stops
1.0 short and 0.9 above the bottom; placing by viewBox would have folded those
margins invisibly into the gap and the vertical alignment. It is also the one
thing upstream's own call site does that is worth keeping outright -- their
`BRAND_MARK_ASPECT` and `BRAND_WORDMARK_ASPECT` are measured off the ink for
the same reason, so that a re-export with padding cannot stretch the lockup.
Measured at 40x with a 50% alpha threshold:

| | ink bbox | aspect |
| --- | --- | --- |
| symbol | 105.100 x 115.900 | 0.90682 |
| wordmark | 340.025 x 77.125 | 4.40875 |

So `0.155306299 = 18 / 115.900` puts the symbol's ink at exactly 18 logical px
tall, and `0.155591572 = 12 / 77.125` puts the wordmark's cap height at
exactly 12. `28.322760 = 6 + 18 x 0.90682 + 6` is the left margin plus the
symbol's resulting width plus the gap. The wordmark's `2.5` is the optical
centre: the symbol's alpha-weighted vertical centroid measures 47.06% down its
own ink, so a 12 px cap centred on it wants 2.470, rounded to the nearest half
logical pixel -- one whole texel at 2x.

What the numbers mean, and why each is what it is, is on
`BrandMark::DEFAULT_HEIGHT_PX` and `brand_mark::GRID_UNIT_PX`; briefly,
everything here is a multiple of a 3 px grid read out of the licence credit's
own plate, the 18 px symbol IS that plate, and the 12 px cap height is that
plate's line box.

### The render

    rsvg-convert -w 1392 -h 288 lockup-{dark,light}ink.svg   # 8x of 174x36
    take the ALPHA channel only, Lanczos to 174x36, clamp to 0..255
    write RGBA with rgb set to the flat ink and that alpha

Supersampling and filtering down rather than rendering straight to 174x36:
measured, the 8x route leaves 19.8% of the inked texels at a midtone alpha
against the direct render's ~31%, i.e. visibly crisper edges for the same ink.

**Resampling only the alpha, and writing the rgb as a constant, is what
replaces the premultiply/un-premultiply dance an early cut needed.** That
dance existed because rsvg writes fully transparent texels as transparent
BLACK and a straight-alpha downscale averages that black into every edge
texel -- a visible grey fringe around the white-ink variant. Here there is no
second colour in the buffer to average toward, because the colour is not in
the buffer at all: it is written afterwards. The fringe is impossible rather
than corrected, and `the_artwork_is_monochrome_by_construction` is what says
so.

174 x 36 is exactly 2x of 87 x 18 with nothing to round.

## The opacity, and why it is not upstream's

`BrandMark::DEFAULT_OPACITY` is **0.80**: the mark is drawn held back from
full strength. The credit in the opposite corner is not held back: it
sits in a MapLibre-style pill (`attribution::AttributionTone::palette`,
white on light maps, `#1d2025` at 90% on dark ones) that guarantees its own
contrast. The mark has no plate to do that, so its figure had to be derived
rather than picked. The constant's own doc comment carries the full
derivation; what belongs here is where the idea and the figure each came
from, because they came from different places.

**The argument is upstream's.** `BRAND_OPACITY` in SWAT's desktop
app's `panels/vehicle_map.rs`: "it is a signature over somebody's map, and at
full strength it competes with the data it sits on." Asked which of their
figures were load-bearing, its maintainers said the argument applies harder
here than there -- they stamp their own app, this crate stamps every
consumer's map -- and in the same breath that the DIGITS behind it are not
evidence: 0.85 was the first value tried, on one dark basemap, over vector
data, with no A/B behind it. Their words: take the argument, not the digits.

**The figure is measured here.** The bar is WCAG AA, 4.5:1 -- the same
standard
`the_attribution_credit_clears_wcag_aa_over_the_darkest_possible_basemap`
holds the licence credit in the opposite corner to. The mark is graded
against the colours the styles in `examples/` actually paint, and against
every FILL rather than only each style's `background` layer, because what is
under the bottom-left corner is a polygon and not the clear. Compositing is
`a x ink + (1 - a) x background` in sRGB-encoded space, which is what the
hardware does (`ALPHA_BLENDING` onto a `Unorm` target; `icon_shader.wgsl`
carries the warning about changing one without the other).

| background | what it is | ink | AA at a >= |
| --- | --- | --- | --- |
| `#88aaa2` | swat-biz-light, darkest fill | dark | **0.756** |
| `#5fd064` | liberty, darkest fill | dark | 0.684 |
| `#a3c1de` | protomaps, darkest fill | dark | 0.679 |
| `#cdd2d4` | protomaps, `background` layer | dark | 0.642 |
| `#1a1e23` | swat-routes-dark, `background` | white | 0.456 |
| `#000000` | `Space::color`, the globe's field | white | 0.455 |

0.756 is the floor, 0.80 is the first twentieth above it, and at 0.80 the
worst shipped case measures 4.93:1 while the two dark ones sit at 11.1:1 and
13.1:1. 0.75 puts the binding case at 4.44:1, under AA. Note which end binds:
the DARK one never does, because white on near-black has four times the
headroom -- which is exactly why a figure arrived at on a dark basemap says
nothing about a light one, and why 0.85 is not the number here.

Cut and looked at as well as computed. The lockup was rendered over
`#88aaa2` at 1.00, 0.90, 0.85, 0.80, 0.75 and 0.70 and read at 1x and 2x: the
wordmark greys out and starts reading as disabled chrome between 0.80 and
0.75, which is the same edge the arithmetic puts at 0.756.

`the_default_opacity_clears_wcag_aa_on_every_shipped_basemap` re-derives every
floor in that table by bisection and both bounds on the constant, so the
figures cannot be written down here and be wrong. It also pins the limit of
the claim: at the crossover grey `#7C7C7C` NEITHER ink clears AA even
undiluted, because a monochrome mark with no plate behind it cannot be
guaranteed over arbitrary raster imagery. A host that knows its own basemap
raises the field.

## Why this replaces a 69x18 lockup, a symbol-only mark, and a lockup before that

This is the fourth artwork in this file's short history.

The FIRST was the SWAT web app's `SwatLogoLight.svg`, the full
pin-plus-"SWAT MOBILITY"-plus-tagline lockup, cropped to 79x25 logical px --
a different asset from the one the desktop app draws, and it read as a
different logo. The SECOND was the desktop app's own symbol ALONE at 23x25,
which fixed that but dropped the wordmark, and at 25 px stood 1.39x the
licence credit's 18 px plate: it overtopped the credit by 7 px and stopped
reading as chrome beside it.

The THIRD put the letters back and got the BOX right -- 69x18, the credit's
plate height exactly -- but not the INK. It carried 3 px of transparent
margin above and below the ink, taken from `ATTRIBUTION_PLATE_PADDING_Y_PX`
by symmetry with the horizontal margin. The horizontal one is load-bearing:
it is what puts the mark's ink 10 px in from the left edge where the credit's
ink is 10 px in from the right. The vertical one was not, and the symmetry
was false. The credit's vertical padding holds its text off its own PLATE
EDGE, a drawn and visible boundary; the mark has no plate, so there was no
edge for a vertical margin to hold anything off. What it did instead was make
the mark smaller than the object it shares the band with: 12 px of ink beside
an 18 px plate, with its lower edge 7 px up from the window where the plate's
was 4. Two corners that agreed on paper and did not read as one band on
screen.

This fourth cut keeps the box and fills it. The symbol is 18 px -- the
credit's whole plate, bottom edge on bottom edge -- and the wordmark's cap
height is 12, the credit's line box, which is the figure the symbol used to
be. Both still read out of the credit's own constants; only the mapping
changed, from ink-to-line-box to ink-to-object.

The cap-to-symbol ratio went from 75% to 67% with it, and that is a choice
rather than a consequence for the first time. 75% was arithmetic with no
freedom in it: 9 px is the wordmark's measured 1x legibility floor and the
symbol was pinned at 12, so 9 of 12 was the only figure available -- the old
doc comment said so, and noted that it sat above the 50-70% a horizontal
lockup usually takes. With the symbol at 18 the floor stops binding, and the
grid offers 9 (50%), 12 (67%) and 15 (83%). All three were cut and looked at:
at 9 the word is a caption under a pin, at 15 the symbol overshoots by only
1.5 px and the lockup flattens into five glyphs, and 12 is the one where the
symbol leads and the word still reads. 13.5 -- 75% held for its own sake --
is off the grid and was the weakest of the four, the word crowding the symbol
at both ends.

The gap stayed at 6 px, two grid units, and that is now a second agreement
with upstream rather than a coincidence: the SWAT desktop app sets its mark
and wordmark in an `h_flex` with `gap_1p5`, which is 6 px, against a 19 px
mark. An 18 px symbol with a 6 px gap is the same proportion they ship.

What it costs, stated rather than dropped: the wordmark's baseline no longer
coincides with the credit's. The old cut's did, because a 9 px cap optically
centred in a 12 px symbol happened to land within 0.22 px of it. A 12 px cap
optically centred in an 18 px symbol lands ~1.72 px below it. Optical centring
inside the lockup and baseline agreement with something 300 px away cannot
both be had; the internal relationship is the one the eye actually reads, and
the two corners now agree on something more visible instead -- their objects'
top and bottom edges. `a_default_mark_stands_in_the_credits_own_band` pins
both the new alignment and the 1.72.

The footprint is 87x18. It is LOUDER in ink than its predecessor -- 31.4% of
the box against 21.8%, 492 logical px of ink against 271 -- and quieter on
screen, because it is drawn at 0.80 rather than at full strength. Against the
one real precedent, Mapbox GL JS's own logo control is 88x23 CSS px
(`.mapboxgl-ctrl-logo`), also a symbol-plus-wordmark lockup: 87x18 is within
a pixel of its width and five short of its height.

## Why a raster, when the source is vector

This crate has no SVG rasteriser it can rely on. `resvg` IS in the dependency
graph -- but underneath `gpui`, which is to say only on the platforms gpui
builds for. The browser build reaches these same sources by `#[path]` with
its own dependency set (`web/Cargo.toml`), where `image` is taken as
`default-features = false, features = ["png"]` and no SVG decoder exists at
all; the iOS shell (`ios/Cargo.toml`) does the same. Adding `resvg`/`usvg` as
a direct dependency of a crate four applications compile, so that one mark
can be drawn, is not a trade worth making -- and a fixed PNG is also the only
form the acceptance pixel test
(`the_brand_mark_draws_bottom_left_and_leaves_the_licence_credit_byte_identical`)
can pin. Upstream can and does use the SVG directly, because gpui rasterises
it for them; that is the one thing about their setup this repository cannot
copy.

The cost of the choice is real, it grew when the wordmark went in, and it is
stated rather than waved away: the mark is RESAMPLED at any scale factor other
than 2x, not re-rendered. On a 2x display it is 1:1 and exact. On a 1x display
it is a bilinear halving, and a halved wordmark is a much more demanding thing
than a halved silhouette.

That halving was measured rather than guessed at, because it used to be what
set the mark's size. Method: render the wordmark at 2x of a candidate cap
height, premultiply, bilinear-halve, and score the result at both sub-pixel
phases on two things that fail before a reader notices anything else --
whether any stroke in the word still reaches full ink, and whether the counter
inside the A stays open.

| cap height at 1x | worst letter's peak alpha | worst A-counter | |
| --- | --- | --- | --- |
| 7 px | 0.82 | 0.25 filled | mush |
| 8 px | 0.85 | 0.22 filled | mush |
| **9 px** | 0.91 | 0.05 | the floor |
| 10 px | 0.94 | 0.13 | |
| 11 px | 0.97 | 0.01 | crisp at every phase |
| **12 px** | -- | -- | what this cut uses |

At 8 and below nothing in the word reaches full ink at any phase and the A
reads as a filled triangle. 9 is where that breaks, and the 69x18 cut sat
exactly ON that floor -- legible and soft rather than crisp on a 1x display,
a known position on a measured curve rather than an oversight, but a cost all
the same. It sat there because 11, which would have been comfortable, did not
fit beside a 12 px symbol.

This cut is at 12, past the crisp threshold at every phase, and that cost is
simply gone. It went away because the SYMBOL grew into the box's dead margin,
not because the box did -- the band the mark shares with the licence credit
is unchanged at 18 px.

A caller who scales the mark down can still walk off the floor: at
`height_px` below 13.5 the cap height drops under 9 px again. Nothing
enforces it, and `BrandMark`'s own doc comment says so.

A hypothetical 3x display gets a 1.5x magnification, which has not been looked
at, because the machine this was built on has only the 2x kind. If either
density turns out to matter the answer is a second file at it beside these two
(`ResolvedVariant::png` is the one place that would change), not a rasteriser.

## Nothing here is Mapbox's, OpenStreetMap's, or maki's

Stated because `examples/assets/PROVENANCE.md` has to state the opposite
about the sheet it describes. These two files were rendered from the
company's own SVGs and contain no traced, derived or downloaded third-party
art.

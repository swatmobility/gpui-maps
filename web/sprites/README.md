# `japan-sprite@2x` — the vendored, license-clean sprite sheet

`examples/mapbox-style.json` declares `"sprite": "mapbox://sprites/mapbox/streets-v12"`.
Resolving that needs a Mapbox access token, and the art behind it is not ours
to redistribute — this repository is public. The other styles name
OpenFreeMap's `ofm` sheet, which is no better.

So this directory holds **our own** sheet. It answers to the same icon
*names* those styles ask for, because a style document requests icons by
name and names are not the art, but every pixel in it is either drawn here
or CC0.

Nothing under this directory is downloaded from `api.mapbox.com` or
`tiles.openfreemap.org`, and nothing is traced from either sheet.

## What ships

| file | what it is |
| --- | --- |
| `japan-sprite@2x.png` | the packed sheet, RGBA, 1022x362 |
| `japan-sprite@2x.json` | the Mapbox-format manifest, 321 entries |
| `maki/*.svg` | the maki icon sources, vendored verbatim |
| `maki/LICENSE.txt` | maki's licence — CC0 1.0 Universal |

`preview.png` and `build_sprites.py` do not ship; `web/build.sh` copies only
the two `japan-sprite@2x.*` files (plus `maki/LICENSE.txt`, as
`dist/LICENSE-MAKI.txt`) into `dist/`.

The `@2x` in the filename is load-bearing: `sprite::sprite_urls` appends
`@2x.json` / `@2x.png` to the base URL in `app.rs::SPRITE_URL`, so those two
names are what the page actually requests.

## Where each part came from

**Route shields, one-way arrows, the city dot — drawn by `build_sprites.py`.**
Plain Pillow geometry: a filleted polygon per shield, supersampled 4x and box
downsampled for clean edges. Ours, no external source.

* `jp-national-route-2` / `-3` — 国道, the blue rounded inverted triangle
  ("おにぎり"), `#0b3f8f`.
* `jp-expressway-2` / `-3` — 高速道路, green rounded rectangle, `#00703c`.
* `jp-prefectural-road-2` / `-3` — 都道府県道, blue hexagon, `#0b3f8f`.
* `default-1` … `default-6` — the fallback badge, slate `#3b4351`. Asked for
  by `examples/mapbox-style.json`, which paints its route number `#ffffff`;
  white ink is why the plate is dark.
* `road_1` … `road_6` — the same silhouette at the same widths, drawn in the
  OPPOSITE polarity: a white plate with a `#3b4351` keyline. `road_N` is
  asked for by `examples/liberty-style.json`, which sets no `text-color`, so
  the spec default `#000000` applies, and dark ink needs a light plate.
  These two names are NOT one badge. Aliasing them to a single drawing,
  which this sheet did until `fix/road-shields-global`, put near-black text
  on slate at **2.1:1** everywhere `road_N` drew a shield — the "dark chips"
  a Hainan frame showed. It also disagreed with the sheet whose names we
  borrow:
  OpenFreeMap's own `ofm@2x` draws `road_3` white with a ~1.5 sheet-px dark
  border, which is exactly why Liberty leaves its shield text black.
* `oneway-small` (8pt), `oneway-large` (12pt), `arrow` (= `oneway-small`) —
  a one-way arrow along +X, `#5a6472` with a white keyline.
* `circle_11_black` — an 11pt black dot with a thin white ring.

maki ships an `arrow.svg` of its own, so `arrow` is a name collision. The
drawn art wins: `build_sprites.py` applies it *over* the maki set rather
than under it. Letting maki win — which it did, until this was caught —
draws every one-way road arrow as a bare 15x15 chevron with no keyline.

**POI icons — the maki set, CC0.** All 215 SVGs, fetched from
`https://raw.githubusercontent.com/mapbox/maki/main/icons/` and vendored
under `maki/` unmodified. `build_sprites.py` rasterizes each to 30x30 sheet
px with `rsvg-convert` and replaces the RGB channels with `#333f4d`, keeping
the alpha, so the glyphs read on the map's light background.

`maki/LICENSE.txt` is maki's own, fetched from the same repo. Its operative
line:

> Affirmer hereby overtly, fully, permanently, irrevocably and
> unconditionally waives, abandons, and surrenders all of Affirmer's
> Copyright and Related Rights and associated claims and causes of action
> […] in the Work (i) in all territories worldwide […] and (iv) for any
> purpose whatsoever, including without limitation commercial, advertising
> or promotional purposes.

CC0 does not require the text to travel with the work. Shipping it anyway is
what makes the waiver checkable by anyone reading `dist/`.

**Aliases — same pixels, extra names.** The styles pick POI icons with
`["get", "class"]`, and OpenMapTiles spells its class values with
underscores where maki uses hyphens. Every maki icon whose name contains `-`
gets a second manifest entry under the underscore spelling, with its own
rect (the engine's atlas packs per name, so an alias cannot be a reference).
Eleven more class values that maki has no icon for by that exact spelling —
`sports_centre`, `ice_rink`, `swimming_pool`, `ferry_terminal`, `office`,
`railway`, `railway_metro`, `subway`, `pub`, `bicycle_rental`, `bus_stop` —
point at the nearest maki icon.

## The manifest format

A flat object, name to rect, exactly what `parse_sprite_manifest` in
`src/sprite.rs` reads:

```json
"jp-national-route-2": { "x": 0, "y": 0, "width": 48, "height": 36,
                         "pixelRatio": 2, "sdf": false }
```

`width`/`height` are **sheet** pixels. `"pixelRatio": 2` on every entry means
the engine halves them to get the size it draws at, so a 48x36 rect is a
24x18 **logical** icon. `"sdf": false` on every entry: these are coloured
images, not distance fields, so they take their colour from their own pixels
and ignore `icon-color`.

No `stretchX` / `stretchY` / `content` — nothing here is used with
`icon-text-fit`. Rects are laid out with a 2 px gutter.

## Sizing the shields

The style paints the route number **on top of** the shield, not into it:
`text-size: 9`, bold, `"text-color": "#ffffff"`, centred on the same anchor
as the icon with no offset. Two consequences drive every dimension here.

1. Every fill has to carry the text its style paints on it. For the four
   dark colours above that means white text, and a white keyline to lift the
   badge off the road casing; `road_N` inverts both, because the two styles
   that ask for it paint their route number black.
2. The part of the shield that has to be wide enough is its **geometric
   centre**, and specifically the band the digits' cap height occupies:
   14 sheet px, 7 either side of centre.

A bold digit of `NotoSansJP-Bold.japan-subset.otf` advances 11 sheet px at
the 18 px em that `text-size: 9` becomes at `pixelRatio: 2`. `road_N`'s width
is `11 * N + 18`, rounded up to an even number so the logical size stays a
whole point.

| icon | sheet px | logical | why |
| --- | --- | --- | --- |
| `default-1` … `-6`, `road_1` … `_6` | 30, 40, 52, 62, 74, 84 x 36 | 15–42 x 18 | rectangle: full width is usable |
| `jp-expressway-2` / `-3` | 44 / 56 x 36 | 22 / 28 x 18 | +4 over the rectangle, because expressway refs are alphanumeric (`E1`, `E1A`) and a capital is wider than a digit |
| `jp-prefectural-road-2` / `-3` | 44 / 56 x 36 | 22 / 28 x 18 | +4: the hexagon has already narrowed by the top of the cap band |
| `jp-national-route-2` / `-3` | 48 / 60 x 36 | 24 / 30 x 18 | +8: the triangle narrows fastest, and only its middle band holds text |

The おにぎり's bottom edge is 64% of its top, with a large fillet. A true
inverted triangle puts the icon's geometric centre — exactly where the style
centres the number — in the narrow tip; at the +4 width the outer digits of
"246" grazed the slope with 1.5 sheet px to spare. Blunting the bottom and
adding 4 more px of width gets that to ~4 px either side while the sign still
reads as an inverted triangle. Height is 36 sheet px (18 logical) for every
shield, which leaves 11 px of slack above and below a 14 px cap band — enough
that where exactly the label renderer centres its line box does not matter.

## Known gap: the subset font has no Latin capitals

`assets/fonts/NotoSansJP-Bold.japan-subset.otf` covers digits and *lowercase*
Latin only (see `assets/fonts/japan-codepoints.txt`: `U+0041`–`U+005A` are
absent). An expressway ref like `E1` therefore shapes as a blank `.notdef`
advance followed by a `1`. `preview.png` shows this deliberately — the
`jp-expressway-2 E1` tile draws only the `1` — with `20` / `302` tiles beside
it so the badge's real width can still be judged. Fixing it means adding
`U+0041`–`U+005A` to the subset, which is a font change, not a sprite one.

## Regenerating

```sh
python3 web/sprites/build_sprites.py
```

No arguments; it reads `maki/*.svg` and writes `japan-sprite@2x.png`,
`japan-sprite@2x.json` and `preview.png` beside itself. Needs Pillow and
`rsvg-convert` (`brew install librsvg`). It never touches the network — the
maki sources are vendored precisely so a rebuild is offline and reproducible.

Layout is deterministic: names are sorted, then stably re-sorted by
descending height for the shelf packer, and both passes are pure functions of
the name set. Two runs produce byte-identical output.

`preview.png` is a review aid, not a deliverable: every icon on mid grey at
its true sheet size, and above that the shields at 3x nearest-neighbour with
a sample number drawn on them in white bold at 9 logical px — the same size,
weight and colour the style uses — so the fit can be judged without guessing.

`web/build.sh` fails the build if the sheet, the manifest or the maki licence
is missing, or if the manifest has lost any of the ten shield and arrow names
the styles resolve to. Regenerate, then run `web/build.sh` before committing.

## What of this sheet can actually draw, on the tiles we serve today

Measured, not assumed — `map_icons_json()` over the demo camera at every
zoom from 11 to 17, and the tile bodies read directly:

* **Route shields draw** — `default-1` … `default-6` under
  `examples/mapbox-style.json`, `road_1` … `road_6` under Liberty, picked
  by `ref_length`.

  The three lines that used to sit here said the shields were
  `jp-national-route-2/-3`, `jp-expressway-2` and
  `jp-prefectural-road-2/-3`, "selected by the style's `match` on the
  `network` tag, which the `transportation_name` layer really carries
  (`jp-prefecture`, `jp-national`, `jp-expressway`)". **That last clause was
  never true.** Read across 34 OpenFreeMap tiles in 24 countries, the whole
  `network` value set is `road` (239 of 252 tagged features),
  `us-interstate`/`us-highway`/`us-state`, `gb-primary`/`gb-trunk`,
  `ca-provincial`/`ca-transcanada`, and absent. No `jp-*` value exists
  anywhere, Tokyo included, so those layers drew **zero** shields — in Japan
  as much as in Singapore. The six `jp-*` rects stay in the sheet: the art
  is fine and a Japan-specific source that emits those networks would reach
  it, but no vendored style names them today.
* **One-way arrows draw**, from z16 up, where the style turns that layer on.
* **`road_1`…`road_3` and `arrow` draw** under the Japan-flavoured style
  this sheet was first measured against, which has since been split into its
  own repository. The names stay in the sheet: a sheet is keyed by name, and
  dropping entries a style might ask for is how an icon silently stops
  drawing.
* **No POI icon can draw at all**, because the tiles carry no `poi` layer.
  At z14 a Tokyo tile has `water`, `waterway`, `landcover`, `landuse`,
  `transportation`, `transportation_name`, `building`, `place` and
  `boundary` — and that is the whole list. So the 215 maki icons plus their
  85 aliases, which are 300 of this sheet's 321 entries, are dead weight on
  *this* dataset: about 75 KB of the PNG and about 1.0 MB of the 1.49 MB
  packed GPU atlas.

They are kept anyway, deliberately. They cost no frame time (an icon that
is never named is never looked up), the styles reference them through
`["get", "class"]` fallbacks that will resolve the moment a `poi` layer is
imported, and re-adding them later would mean regenerating and re-reviewing
the sheet. If the payload ever matters more than that, `build_sprites.py`
takes the icon list from one place and a shields-only sheet is a one-line
change — but do it knowing it is a tile-pipeline gap being papered over,
not a renderer one.

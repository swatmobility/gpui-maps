# Three styles on the glass, and why the map still looks bare

The report was "the colours are very pale still", and the ask was for Mapbox
styles and Liberty — more than one, switchable. This is what shipped, what it
is measured to be worth, and what it deliberately does not fix.

## What shipped

**Three fingers on the glass cycles the style**, in place, with no restart and
no refetch: `mapbox-omt` → `liberty` → back. The active one is
named in the device log on every switch, with its layer count, how long it
took to compile, and a one-line note on what makes it different.

```
[map]   style 1/2: "mapbox-omt", 64444 B, 86 layers compiled in 2 ms -- ...
[style] 2/2 "liberty": 108 layers in 4 ms, 16 meshes dropped, 16 tiles kept decoded -- ...
```

(The cycle was three long when these lines were captured, hence `1/3`; the
third style left with the demo repository it belonged to. The `n/N` is read
from `STYLES.len()`, so the log follows the set rather than this document.)

The switch is cheap because of where the engine already draws its seam.
`decoded` holds MVT bytes, which know nothing about any style; `gpu_tiles`
holds meshes made of the previous style's colours and widths, which are
therefore all stale at once. So a switch is: recompile, recolour the clear,
drop every mesh. Nothing leaves the device. `bake_some` rebuilds the cover
from bytes already in hand, at the same budget a zoom-bucket crossing uses —
about two frames for a twelve-tile cover.

### Why three fingers, and why on the finger's way DOWN

One finger is pan and two are pinch/rotate, so three was the first free
contact. It was also genuinely free rather than merely unused: `pinch_state`
builds its sample from `touches[0]` and `touches[1]` only, so a third finger
previously did nothing at all, and giving it a meaning takes nothing away
from `ROTATION.md` or `PINCH-FLING-FIX.md`.

It fires on the third finger's `Started`, not on a lift, because a tap needs
a timeout to be recognised and the two fingers already down are mid-pinch.
Waiting `TAP_MAX_MS` to find out means 300 ms of the map zooming and spinning
before it turns out the user wanted neither. Firing immediately and then
latching `multi_touch_consumed` for the rest of the gesture is the only
ordering in which the map does not move at all — including through the
one-finger-at-a-time lift at the end, whose last contact would otherwise read
as a tap and pair into a double-tap zoom.

Seven tests in `app::style_switch_tests` drive `on_touch` directly (the
Simulator cannot synthesise a third contact): the cycle wraps, the live style
and its clear colour both change, no decoded tile is evicted, a deliberately
wobbly three-finger hand moves the camera by exactly nothing and flings
nothing, the trailing lift is not half a double-tap, and a one-finger pan
immediately afterwards still pans.

## What the numbers say about "pale"

`ios/examples/style_audit.rs` is the harness the parent repo's
`examples/style_paint_audit.rs` wanted to be and never ran — same question,
but inside the iOS crate, whose whole dependency graph is the tessellator plus
winit rather than gpui + tokio + reqwest + 1700 others. Twelve real
OpenFreeMap tiles (Tokyo, Osaka, Singapore at z6/z10/z12/z13/z14, and the z14
ones re-resolved at z17, which is where a phone actually sits over a z14
source).

Paint ops — everything this build can currently put on the glass:

| tile | `mapbox-omt` fill/line | `liberty` fill/line |
| --- | --- | --- |
| tokyo z14 | 547 / 1062 | 660 / 1593 |
| singapore z14 | 970 / 3540 | 1619 / 5791 |
| osaka z12 | 132 / 3918 | 134 / 9185 |
| singapore z6 | 28 / 16 | 169 / 316 |

**Liberty is not drawing less. It draws more.** Both of these styles put
thousands of ops on a dense tile, and Liberty puts down the most of the set
at almost every zoom (it did so against the departed third style too, whose
column this table used to carry). The palish look is entirely palette — `#f8f4f0` ground,
near-white landuse, white roads — and not a renderer or a schema failure. That
is worth stating because it means a style change genuinely can fix it, and
this is the change: `mapbox-omt` leads the cycle precisely because it is the
saturated one (`#99ddff` water against Liberty's washed
`rgb(158,189,255)`, `#ffb266` motorways, `#f7e06e` trunks, `#b8ebad` parks).

## What the numbers say about "bare"

Same harness, symbol side. `mapbox-omt` over Tokyo z14:

| | z14 | z14 resolved at z17 |
| --- | --- | --- |
| symbol features | 190 | 2414 |
| carrying text | 188 | 1231 |
| carrying an icon | 76 | 2119 |
| would-be icon quads, our sheet | 76 | 2023 |

**None of that is drawn.** Not one label and not one icon, in any style, on
this build. Two separate reasons, and it matters that they are separate:

1. **No symbol pass exists on iOS at all.** Nothing in `ios/src/app.rs` calls
   `style::match_tile_symbols`, and `ios/src/render/mod.rs` deliberately drops
   `icon_pipeline`. So the diagnosis that `mapbox-style.json`'s
   `sprite: mapbox://sprites/mapbox/streets-v12` needs a token and therefore
   fails is true on the desktop and **moot here** — iOS never calls
   `sprite::resolve_sprite_url` at all, on any style. Repointing that URL, to
   our sheet or anyone's, would change exactly nothing on device.
2. **No labels, one layer further down.** `font_fallback`'s `new_font_system`
   calls fontdb's `load_system_fonts`, which has no iOS branch: iOS is `unix`
   and not `macos`, so it takes the LINUX path to `/usr/share/fonts` and
   friends, none of which exist in an app sandbox, and the database comes back
   empty. Already documented in `ios/src/lib.rs`; still queued.

So of what is missing from the screen right now, in feature counts over a
dense city tile at the zoom a phone actually uses: roughly 1200 labels and
2100 icons per tile. Text is the larger half and is the harder fix. A style
change moves the palette and nothing else — that is its whole scope, and it
is worth being clear that the map will still read as bare after picking a
favourite, because a basemap without any typography does.

## The sprite sheet is ready and cannot be used yet

`ios/assets/japan-sprite@2x.{png,json}` is vendored (see its `PROVENANCE.md`):
321 entries, token-free, CC0 maki POI icons plus drawn Japanese route shields,
and it answers to **both** naming conventions the three styles use. Measured
against real tiles it covers 96% of icon-bearing features in all three, where
the OpenFreeMap sheet already vendored under `examples/assets/` covers 57% of
`mapbox-omt`'s — it has no hyphenated maki names and no `oneway-small`, which
alone costs 4480 one-way arrows.

### What stands between this and a drawn icon

**An engine change, which this branch is not allowed to make.** Stopping here
and reporting it is the instruction, so:

`src/render/icon_pipeline.rs` — the pipeline, the shader, `IconInstance`, the
packer — is fine on iOS and needs nothing. But it names `crate::sprite` in
`pack_icon_atlas`, so `src/sprite.rs` has to exist in the `#[path]` closure,
and `src/sprite.rs` names `reqwest::Client` in two functions:

```rust
pub async fn fetch_sprite_atlas(base_url: &str, http: &reqwest::Client) -> ...
async fn fetch_bytes(http: &reqwest::Client, url: &str) -> ...
```

A `#[path]` include takes the whole file, so including it means putting
`reqwest` — and with it hyper and a tokio runtime — into an iOS crate whose
stated reason for existing is that it does not carry them. Verified, not
assumed: adding `#[path = "../../src/sprite.rs"] pub mod sprite;` to
`ios/src/lib.rs` fails with three `E0433 cannot find module or crate
reqwest`.

The fix is two lines, and **the wasm spike has already written one of them**.
`gpui-map-web2`'s `wasm-presentable` branch carries exactly this, for exactly
this reason:

```rust
/// `cfg`-gated on the target, not on a feature ... `reqwest` has no
/// `wasm32-unknown-unknown` build ... Everything else here -- `slice_atlas`,
/// `sdf_from_mask`, the manifest parser -- is arithmetic over bytes already
/// in hand and crosses untouched.
#[cfg(not(target_arch = "wasm32"))]
```

Widening that predicate to `#[cfg(not(any(target_arch = "wasm32", target_os =
"ios")))]` — or, better, replacing both cases with a single `http` feature —
unblocks iOS. Nothing else in `src/` needs to move for the pipeline itself.

The rest of the work is then host-side and lives in `ios/`, not in the engine:
about 150 lines porting six small helpers out of `map_view.rs`
(`project_symbol_anchor_with_matrix`, `symbol_angle_rad`, `blended_icon_size`,
`icon_logical_size`, `sdf_paint_for`, `icon_font_scale`) plus a per-frame loop
that walks each visible tile's `SymbolPlan` and builds `IconInstance`s. That
is the same seam `web2` cut at, though it went further and lifted the whole
pass into a new `src/symbol_pass.rs` (2069 lines) that iOS could then share
rather than re-port — which is the better end state and a much larger change
than two `#[cfg]` lines.

One warning for whoever does it: the z17 column above says a dense tile
resolves 2100–6500 icon-bearing features. Twelve of those in a cover is tens
of thousands of quads with nothing arbitrating between them, because
collision placement lives in `map_view.rs` too. An icon pass without a
collision pass is not a partial win at street zoom; it is a solid mat of
overlapping sprites. Either port `place_visible_symbols` with it or clamp the
first cut to point features below z15.

## A small engine bug the audit turned up

Not fixed here (`src/` is frozen on this branch), but worth recording. 45
features per fixture set resolve their `icon-image` to the **empty string**
and are emitted as `icon: Some("")` — an atlas lookup that must miss on every
feature, every frame.

`style.rs` is explicit that this must not happen:

```rust
/// `""` means "no icon", never an icon literally named "".
fn non_empty(name: String) -> Option<String> { ... }
```

`DataIconShape::resolve` applies it. `IconImage::resolve` does not:
`IconImage::ZoomStep(zv) => Some(zv.value_at(zoom))` returns whatever the zoom
step holds, and Liberty writes
`["step", ["zoom"], "circle_11_black", 10, ""]` on `label_village`,
`label_town`, `label_city` and `label_city_capital` — the standard "show a dot
below z10, let the text stand alone above it". `IconImage::Get` and
`IconImage::Concat` have the same gap for a tag whose value is empty.
`mapbox-omt` has no such step and shows none of them, which is why this only
appears once the switcher put another style under the same harness.

## The Mapbox-token option — reported, not implemented

A token was available locally. It was not read, not printed, not copied and
not embedded, and nothing in this commit derives from it. What it would take:

**A token alone buys nothing.** The four genuine Mapbox styles in `examples/`
(`mapbox-streets-style.json`, `mapbox-standard-dark-style.json`,
`swat-biz-light-style.json`, `swat-routes-dark-style.json`) are written
against the **Mapbox Streets v8** schema — `road`, `place_label`, `structure`
— and `app::TILE_URL` serves OpenMapTiles, where those layers are called
`transportation`, `place` and `building`. `style::match_tile` matches by name,
so those styles compile perfectly and then draw a coloured rectangle: of
`swat-routes-dark`'s 50 layers, 5 bind and 44 do not, and all 29 dead LINE
layers are `source-layer: "road"`. So this is a `TILE_URL` change as much as a
credential one — `https://api.mapbox.com/v4/mapbox.mapbox-streets-v8/{z}/{x}/{y}.vector.pbf?access_token=…`,
which puts every tile request, not just the sprite fetch, behind the token and
onto metered Mapbox quota.

**Injecting it safely is easy; the risk is what makes it worth thinking about
anyway.** `option_env!("MAPBOX_TOKEN")` reads an environment variable at
COMPILE time and bakes the value into the binary, so the token never enters a
source file and never enters git — `run-device.sh` would grow one exported
variable and nothing else. `sprite::resolve_sprite_url` already takes an
`Option<&str>` token and already `bail!`s rather than fetching when it is
absent, and `sprite::redact_access_token` already exists to keep a `pk.` value
out of a logged error string, so the plumbing is built.

The risk is not the mechanism, it is the artefact. **This repository is
public.** A token in a committed `.env`, a shell script, a test fixture, a
CI file or a pasted log is a live credential on someone else's bill, and
Mapbox tokens do not expire on their own. `option_env!` keeps it out of the
source tree; it does not keep it out of a `cargo build` invocation someone
pastes into an issue, or out of the compiled binary itself, which anyone
holding the `.ipa` can read with `strings`. That last one is worth saying
plainly: a token in a shipped app is a published token, which is why Mapbox
scopes them by URL restriction rather than pretending they are secret.

Given the vendored sheet above covers 96% of every style's icons with no
credential at all, and given that the token would additionally move the whole
tile source onto a quota, the recommendation is to leave `TILE_URL` on
OpenFreeMap and finish the sprite path instead.

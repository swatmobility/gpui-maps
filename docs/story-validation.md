# Story validation

`story::validate_scene` (the `story` example's `--validate-only`, and the
agent action `story.validate`) checks a story script before a render and returns
one report:

```json
{"ok": true, "diagnostics": [...], "estimate": {...}, "ffmpeg": {...}, "design": {...}}
```

Each diagnostic has a `severity` (`error`, `warning` or `info`), a JSON
pointer `path` to the part of the script that caused it (`/events/3`,
`/chapters/1`, `/output/safe_area`), a stable `code`, a human `message`
and, usually, a `hint`. `ok` is `false` only when a diagnostic is an
error: warnings never block a render.

## The design report (spec §11, R10)

`design` is `null` for a 1a story (no top-level `design`), which draws
exactly as before. Otherwise it is the design system's own decisions, in
plain English, never a Rust type name:

```json
{"archetype": "journey", "rule": 5, "theme": "ember", "tone": "paper", "safe_area": "none", "map_share_min": 0.86}
```

`archetype` and `rule` are the detected archetype and which spec §2.1
rule (1-7) decided it; `theme` and `tone` are
the overlay theme and the card's fill; `safe_area` is `output.safe_area`
(`none` when absent, D1); `map_share_min` is the lowest share of the
visible frame any card on screen leaves to the map (spec §5.6) -- below
0.75 is `card_too_tall`.

## Layout warnings

Validation lays the story out the way the render does, with no GPU, on a
bounded set of frames:

- each panel and image: its first frame, its first fully faded-in frame
  (`at + fade_s`), its midpoint and its last frame (at most four; they do
  not move, and each is also laid out at every other element's frames);
- each callout: every frame of its span, because its anchor moves with
  the camera -- 4,000 callout frames in all, on an even stride beyond
  that -- plus the frames either side of each side flip and visibility
  change of its schedule;
- each chapter: its first frame and its still; each caption: its
  faded-in frame and its midpoint.

Each layout warning names the event that caused it. A caption names its
own `caption` event (`/events/<i>`) or `captions` cue (`/captions/<j>`);
map furniture names `/furniture` or the chapter's own
`/chapters/<i>/furniture`; a frame with no room at all names `/output`.
A warning is reported once per path and code, at the earliest sampled
time, and its message starts with that time (`at 0.40 s: ...`).

**Fonts.** Validation measures text with the shipped label faces, which
is what every render draws with -- unless the `story` example renders
with `--system-fonts`, which swaps in the machine's own faces after
validation. Text can then wrap differently, so a layout warning may
appear or vanish in that render.

**Alt text.** A chapter's alt text is its own `alt`, or else the story's
`summary`; blank text counts as none. `chapter_without_alt` fires when a
chapter has neither (at `/chapters/<i>`, or at `/summary` for a script
without `chapters`), and the transcript and the manifest's alt-text
coverage use the same rule.

## The estimate, and how long validation takes

`ValidateOptions.estimate` plans every frame's tiles for the render
estimate (`estimate` in the report, and the `cover_too_large` warning):
the chosen output every frame, the other tiers every half second. The
`story` CLI always estimates. The agent's `story.validate` does not
unless asked (`"estimate": true`): it answers `estimate: null` and
quickly. On a debug build, a 10-minute 25 fps story with 40 panels, 10
captions and 10 ten-second callouts (2,650 of its 15,000 frames laid out)
validates in about 0.2 s without the estimate and 2.8 s with it.

## Every warning code

The same table is in `src/story/validate.rs`'s module documentation and
in `WARNING_CODES`; a test keeps the three equal.

| Code | Severity | Raised by | Meaning |
| `text_shrunk` | warning | layout | An element's text was set smaller than asked to fit its card (never below the 28-unit floor). |
| `text_truncated` | warning | layout | At the size floor the text still did not fit; it ends with an ellipsis. |
| `text_dropped` | warning | layout | A text block had no room at all, not even for an ellipsis, and is not drawn. |
| `in_safe_zone` | warning | layout | No position clears the platform's safe-area rectangles (`output.safe_area`); the element sits under the app's UI. |
| `overlap` | warning | layout | No position clears another element or the corner chrome (credit plate, SWAT mark, wordmark). |
| `bad_measure` | warning | layout | Measuring the element's text gave a NaN, infinite or negative size; it was taken as 0. |
| `bad_input` | warning | layout | A caption's size or offset is not a finite number; the floor or 0 is used. |
| `callout_offscreen` | warning | layout | A callout's anchor did not project to a finite point; it is not drawn. |
| `frame_too_small` | warning | layout | The frame has no room inside its margins and safe areas; no story element is drawn (path `/output`). |
| `scale_bar_hidden_pitch` | info | layout | The scale bar was asked for but the camera is pitched too far for one scale; it is hidden on that frame. |
| `missing_alt` | warning | images | An image has no `alt` text. |
| `missing_credit` | warning | images | An image has no `credit`: the render is not publishable. |
| `animated_image_first_frame` | warning | images | An animated image draws only its first frame. |
| `safe_area_mismatch` | warning | validate | `output.safe_area` names an app whose UI was measured for another aspect; no safe-area rectangles apply. |
| `chapter_without_alt` | warning | validate | A chapter has no `alt` (the implicit chapter of a script without `chapters` counts the story's `summary`). |
| `raster_upscaled` | warning | validate | The raster has no @2x tiles at a pixel ratio above 1; its tiles are drawn upscaled. |
| `cover_too_large` | warning | validate | One frame covers more than 150 tiles: slow and memory-hungry. |
| `gpu_memory` | warning | validate | The frame exceeds the capture device's texture or buffer limit, or the GPU memory estimate exceeds 2 GB. |
| `too_many_features` | warning | validate | An overlay has more than 50,000 features. |
| `hairline` | warning | validate | A line layer's width is under 2 units. |
| `unsupported_paint` | warning | validate | An overlay layer names paint properties the story renderer ignores. |
| `highlight_over_gradient` | warning | validate | A highlight targets a layer drawn with `line-gradient`, which it cannot recolour. |
| `legend_unsupported` | warning | furniture | A legend's layer colour is an expression the legend cannot list. |
| `area_property_unmatched` | warning | validate | No feature of an area layer has the categorical legend's `property` equal to an item label: every area would be drawn "Other". |
| `area_property_not_numeric` | warning | validate | No feature of an area layer has a number in the sequential legend's `property`: the area is drawn in one colour. |
| `legend_overridden` | warning | validate | A legend's own values for an area are replaced by an earlier legend's, which both cards and the fill show. |
| `legend_field_unused` | info | validate | A legend field that changes nothing: `ramp` on a sequential legend, `stops` on a categorical one without `ramp`, `property` on a card legend naming no area, a `legend` on a layer that is not an area. |
| `chapter_past_end` | warning | chapters | A chapter's `to` is past the story's end; it is clamped. |
| `features_without_id` | warning | timeline | A highlight's `ids` cannot name the layer's features that have no GeoJSON `id` while others do. |
| `unknown_feature_id` | warning | timeline | A highlight's `ids` names a feature the layer does not have. |
| `highlight_base_color` | warning | timeline | Highlighted features have no colour to ramp from; they switch to the highlight colour at once. |
| `sun_time_without_terminator` | warning | script | `sun.time` has no visible effect without the terminator. |
| `empty_attribution` | warning | script | The basemap states no credit: the render is not publishable. |
| `mapbox_wordmark_missing` | warning | script | A Mapbox basemap has no `wordmark`: the render is not publishable. |
| `until_ignored` | warning | script | `until` on a `show`/`hide` effect is ignored; it ramps over `fade_s`. |
| `lon_wrapped` | warning | script | A longitude outside +/-180 is wrapped. |
| `ffmpeg_found` | info | validate | The ffmpeg a video render would use, and its version. |
| `ffmpeg_missing` | info | validate | No ffmpeg: a video render writes numbered PNGs and an `encode.sh` instead. |
| `safe_area_not_chosen` | info | validate | A design story names no `output.safe_area`; the card sits near the bottom edge (D1). |
| `extra_supporting_item` | warning | validate | A card has more than one supporting item (legend, stats, photo, a quote's attribution); only the first (spec §3.2's order) is shown. |
| `headline_ignored` | warning | validate | A card has both a `quote` and a `headline`; the quote is shown. |
| `supporting_item_deferred` | warning | validate | A card's first row needs three lines, which drops its supporting item. |
| `card_too_tall` | warning | validate | The card leaves less than 0.75 of the visible frame to the map (spec §5.6). |
| `too_many_stats` | warning | validate | A card names more than two stats; only the first two are drawn. |
| `legend_crowded` | warning | validate | A card's legend has more than four items: it draws smaller, in three columns. |
| `legend_too_many_items` | warning | validate | A card's legend has more than six items: the rest are grouped as "Other". |
| `sources_too_long` | warning | validate | The colophon row's sources need more than two lines; one group is left out. |
| `accent_low_contrast` | warning | validate | The design accent is under 3:1 against a `paper` card. |
| `index_without_tour` | warning | validate | A card's `"index": true` has no `numbered` layer (not a `tour`) to count against. |
| `unknown_dir` | warning | validate | `design.dir` is neither `ltr` nor `rtl`; the direction follows `lang`. |
| `unknown_credits` | warning | validate | `design.credits` is neither `bottom` nor `above_platform`; the credits sit on the bottom row. |
| `label_under_card` | warning | validate | A basemap label would sit behind the card (or the colophon row) on either side, so it is hidden on this frame. |
| `story_labels_capped` | warning | validate | More than 64 story labels would draw on this frame; the rest are left out. |

Errors (unknown fields, bad references, missing or escaping files,
effects on the wrong layer type, an unknown safe area, aspect or tier)
refuse the render; spec §7 lists them.

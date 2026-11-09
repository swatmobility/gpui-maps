# The page's label faces

Four files, two pairs, both OFL 1.1 — see `LICENSE-OFL.txt`, which names
both copyright holders because they differ.

| file | family | codepoints | bytes |
|---|---|---|---|
| `NotoSansJP-Regular.japan-subset.otf` | Noto Sans JP | 2,836 | 691,816 |
| `NotoSansJP-Bold.japan-subset.otf`    | Noto Sans JP | 2,836 | 710,232 |
| `NotoSans-Regular.latin-subset.ttf`   | Noto Sans    |   911 |  70,976 |
| `NotoSans-Bold.latin-subset.ttf`      | Noto Sans    |   911 |  70,876 |

Five more files live in this directory for the story design system's card
(spec §4.1, D5) — see "The story faces" below. They are **not** in the
table above because `web/build.sh` never ships them: they are loaded only
by `font_fallback::STORY_FONTS`, native-only (`cfg(not(any(target_os =
"ios", target_arch = "wasm32")))`), so the browser and iOS shells pay
nothing for them.

`app.rs::FONT_URLS` fetches all four into one `fontdb::Database`;
`app.rs::FONT_FAMILY` (`Noto Sans JP`) is what every `text-font` stack
resolves to, so the Latin pair is reached per-glyph through
`font_fallback`'s `COMMON_FALLBACK`, which already named `"Noto Sans"`.

## Why the Latin pair exists

The Japan subset was cut from the codepoints in a Japan map-feature table
(`japan-codepoints.txt`). Japanese place names contain **no ASCII
capitals**, so all 26 of `U+0041`–`U+005A` are absent from it, along with
lowercase `b d g h k m q v z`. On Japanese map text that is invisible. With
`?tiles=` pointed at OpenStreetMap it is ruinous: "Singapore" drew as
"in apore", and every expressway shield (AYE, ECP, CTE, PIE, …) drew as an
empty plate.

`sprites/README.md` had already recorded this for the sprite sheet's shield
text — "Known gap: the subset font has no Latin capitals" — and said the fix
"is a font change, not a sprite one". The Latin pair is that change. It is
**added** to the Japan pair, never substituted for it, so Japanese and the
lowercase and digits the Japan subset already covers still render from the
Japan faces exactly as before.

## Regenerating the Latin subset

Upstream is Noto Sans (Latin) **version 2.000**, `Copyright 2015 Google Inc.`
— `github.com/notofonts/latin-greek-cyrillic`, also redistributed in Google
Fonts and vendored by several Rust/Go UI toolkits. The pair used here came
from a local copy of that vendored file; any upstream `NotoSans-Regular.ttf`
/ `NotoSans-Bold.ttf` at version 2.000 gives the same result.

```sh
UNICODES='U+0020-007E,U+00A0-00FF,U+0100-017F,U+0180-024F,U+02BB-02BC,U+0300-036F,U+1E00-1EFF,U+2010-2015,U+2018-2019,U+201C-201D,U+2026,U+2032-2033,U+20AB'
for w in Regular Bold; do
  python3 -m fontTools.subset "/path/to/NotoSans-$w.ttf" \
    --unicodes="$UNICODES" \
    --layout-features='kern,liga,ccmp,mark,mkmk,locl' \
    --name-IDs='0,1,2,3,4,5,6,13,14' \
    --no-hinting --desubroutinize \
    --output-file="NotoSans-$w.latin-subset.ttf"
done
```

The ranges are Basic Latin, Latin-1 Supplement, Latin Extended-A and -B
(so European diacritics work, not just English — Romanian ș ț are -B),
the combining diacritics `U+0300`–`036F` (for names a tile ships
decomposed), Latin Extended Additional `U+1E00`–`1EFF`, the đồng sign, and
the punctuation place names actually use — dashes, curly quotes, ellipsis,
prime marks, the ʻokina.

Extended-B and Extended Additional are Vietnamese. The first cut stopped
at Extended-A, and over Ho Chi Minh City "Thủ Dầu Một" drew as
"Th D u M t": ă and Đ (Extended-A) survived, while the horned ơ ư
(`U+01A1`, `U+01B0`) and every stacked-diacritic letter (`U+1EA0`–`1EF9`)
left a blank advance. No other shipped face covers them, and the Latin
script has no per-script fallback (`font_fallback::script_fallback_families`),
so the glyph was simply not drawn. `font_fallback`'s
`every_char_of_a_vietnamese_name_has_a_glyph_in_the_*_font_set` tests
shape Vietnamese names against these exact files. `name-IDs`
keeps `0` and `13`/`14`, i.e. the copyright and licence records, because
`LICENSE-OFL.txt` quotes nameID 0 verbatim and a subset that dropped it
would make that quote unverifiable.

`build.sh` asserts on the **shipped** files that every capital and all nine
of `b d g h k m q v z` are present, plus ơ ư Ơ Ư and all of
`U+1EA0`–`1EF9`, so a subset regenerated with the wrong
`--unicodes` fails the build instead of silently un-lettering the map.

## The story faces (spec §4.1, D5)

Five files, both OFL 1.1, native-only (`font_fallback::STORY_FONTS`): the
story card draws its text in them. `web/build.sh` never touches this
section's files, and iOS does not build them in either (ruling P6) — the
browser and the phone pay nothing for the story card's type.

| file | family | weight | bytes |
|---|---|---|---|
| `SourceSans3-Regular.latin-subset.ttf`         | Source Sans 3        | 400 |  95,256 |
| `SourceSans3-Semibold.latin-subset.ttf`        | Source Sans 3        | 600 |  94,988 |
| `SourceSans3-Bold.latin-subset.ttf`            | Source Sans 3        | 700 |  94,732 |
| `IBMPlexSansArabic-Regular.arabic-subset.ttf`  | IBM Plex Sans Arabic | 400 | 113,816 |
| `IBMPlexSansArabic-Bold.arabic-subset.ttf`     | IBM Plex Sans Arabic | 700 | 116,856 |

Combined: 515,648 bytes (≈503.6 KB). The spec and the plan originally
estimated "~350 KB"; that number was never measured against the real
upstream releases. Measured with `--no-hinting` (below), the real total is
≈500 KB, and both documents are corrected to say so — see the note this
task added at spec §4.1/D5 and the plan's Global Constraints. The ~0.5 MB
difference is native-only: it lands in a roughly 70 MB desktop/capture
binary and never reaches the browser or iOS bundle. The bundled test
(`font_fallback::the_story_faces_are_bundled_at_their_weights_and_stay_small`)
caps the combined total at 520 KB, a small margin over the measured
figure rather than an invitation to add a weight no card token needs.

### Source Sans 3

Upstream: Adobe's `adobe-fonts/source-sans` release **3.052R** (TTF
static), `TTF-source-sans-3.052R.zip`. SHA-256 of the three upstream
files used:

| upstream file | SHA-256 |
|---|---|
| `SourceSans3-Regular.ttf`  | `4644c81b86ec9caaa76b634889968ed3c4f4f52f054855933acc7c2b21e53b0f` |
| `SourceSans3-Semibold.ttf` | `a3f4f8dcf343a8f24dc61951de93f3ba1558b15cd250ba24af8a40e957081b7d` |
| `SourceSans3-Bold.ttf`     | `9214b9d95e4231c609802815c2646c98174e2102d0d37f88978a7f8e71006e6a` |

Fetched with the project User-Agent (`gpui-map
(+https://github.com/swatmobility/gpui-maps)`), no email or token. Subset
command (same for Semibold and Bold, substituting the weight in the
input and output file names); `--no-hinting` drops the TrueType hint
bytecode (`cvt `/`fpgm`/`prep`) that glyphon/cosmic-text never runs
(outlines are rasterized, not hinted), for a lossless ≈29% size cut.
`--name-IDs+=16,17` is load-bearing, not cosmetic: Semibold is not one of
the four RIBBI subfamily names, so its OpenType family (nameID 1) is
"Source Sans 3 Semibold", and the typographic family that groups it with
Regular/Bold under plain "Source Sans 3" lives only in nameID 16/17
(preferred family/subfamily). `pyftsubset`'s default `--name-IDs` keeps
`0`–`6` and drops 16/17, which left the subsetted Semibold with no face
named "Source Sans 3" at all -- `font_fallback`'s
`the_story_faces_are_bundled_at_their_weights_and_stay_small` test caught
it (weight 600 missing from `STORY_FAMILY`) before any font file shipped:

```sh
pip3 install --user fonttools 2>/dev/null || true
pyftsubset SourceSans3-Regular.ttf --unicodes="U+0000-024F,U+0300-036F,U+0370-03FF,U+0400-04FF,U+2000-206F,U+20AC,U+2116,U+2190-2193,U+2212,U+2032-2033" \
  --layout-features='kern,liga,tnum,lnum,locl,mark,mkmk' --no-hinting --name-IDs+=16,17 \
  --output-file=assets/fonts/SourceSans3-Regular.latin-subset.ttf
# same for Semibold, Bold
```

The ranges cover Latin (Basic + Latin-1 Supplement + Extended-A/-B),
combining diacritics, Greek and Cyrillic, so one family draws every
Latin, Greek and Cyrillic run the story card needs (spec §4.1). `kern`,
`liga`, `tnum` and `lnum` are kept for R12's tabular-numeral and
ligature-sensitive card text; they are also the single largest
contributor to each file's size (OpenType class-based kerning spans
every included glyph), which is why the three Source Sans weights alone
are ≈95 KB each rather than the tens of KB a Latin-only subset would be.

### IBM Plex Sans Arabic

Upstream: `IBM/plex` release tag **`@ibm/plex-sans-arabic@1.1.0`**,
asset `ibm-plex-sans-arabic.zip`. SHA-256 of the two upstream files used:

| upstream file | SHA-256 |
|---|---|
| `IBMPlexSansArabic-Regular.ttf` | `8e0f1046c736bf939d4939ee3ae0116acf61cbcd6592deae7656761627080981` |
| `IBMPlexSansArabic-Bold.ttf`    | `b74f809dead12442ed56e02a12c3bcc02076c9ad4e32f17d0a9ca6fc1aafc89e` |

Subset command (same for Bold):

```sh
pyftsubset IBMPlexSansArabic-Regular.ttf --unicodes="U+0000-007F,U+0600-06FF,U+0750-077F,U+FB50-FDFF,U+FE70-FEFF,U+2066-2069" \
  --layout-features='*' --no-hinting --name-IDs+=16,17 \
  --output-file=assets/fonts/IBMPlexSansArabic-Regular.arabic-subset.ttf
# same for Bold
```

Plex Arabic's Regular and Bold are both RIBBI-named already (nameID 1 is
plain "IBM Plex Sans Arabic" for both), so `--name-IDs+=16,17` is a no-op
here; it is kept for consistency with the Source Sans 3 command above.

`--layout-features='*'` keeps every OpenType feature, which for Arabic is
load-bearing, not decorative: initial/medial/final/isolated joining
forms are reached only through GSUB substitution, never through `cmap`,
so a narrower feature list (or `--no-layout-closure`, tried and rejected
during R1) drops the joining-form glyphs those rules point at and the
text renders as disconnected isolated letters. The Arabic presentation
forms ranges (`U+FB50-FDFF`, `U+FE70-FEFF`) are kept for the same reason:
some real-world Arabic text is pre-composed to those codepoints rather
than relying on the shaping engine.

Both subset commands were run with `python3 -m fontTools.subset`
(fontTools 4.62.1), the CLI `pyftsubset` wraps.

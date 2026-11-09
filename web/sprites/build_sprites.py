#!/usr/bin/env python3
"""Builds `japan-sprite@2x.png` / `.json` -- the vendored, license-clean
sprite sheet the browser demo draws its icons from.

Run it with no arguments; it reads and writes only its own directory:

    python3 web/sprites/build_sprites.py

Two sources go in, and nothing else:

* `maki/*.svg` -- the Mapbox maki icon set, CC0 (see `maki/LICENSE.txt`),
  vendored verbatim and rasterized here by `rsvg-convert`.
* The shield/arrow art below, drawn from scratch by this script with
  Pillow. No Mapbox or OpenFreeMap sprite PNG is downloaded, copied, or
  traced -- only the icon NAMES are shared with those sheets, because the
  style documents ask for icons by name.

Everything is emitted at `pixelRatio: 2`: a rect's `width`/`height` in the
manifest are SHEET pixels and the engine divides by two to get the logical
size it draws at (`src/sprite.rs`, `map_view::icon_logical_size`).

Determinism: the icon list is sorted before layout and the shelf packer is
a pure function of that order, so a re-run reproduces the same sheet.
"""

from __future__ import annotations

import json
import math
import os
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
MAKI_DIR = os.path.join(HERE, "maki")
REPO = os.path.abspath(os.path.join(HERE, os.pardir, os.pardir))
BOLD_FONT = os.path.join(REPO, "assets", "fonts", "NotoSansJP-Bold.japan-subset.otf")

SHEET_PNG = os.path.join(HERE, "japan-sprite@2x.png")
SHEET_JSON = os.path.join(HERE, "japan-sprite@2x.json")
PREVIEW_PNG = os.path.join(HERE, "preview.png")

# Every rect in this sheet is a plain RGBA icon at retina density.
PIXEL_RATIO = 2
GUTTER = 2
MAX_SHEET_WIDTH = 1024

# 4x supersample, then an exact box downsample. `ImageDraw` has no
# antialiasing of its own, so this is where clean edges come from.
SS = 4

# --- palette ---------------------------------------------------------------
# A `default-N` shield fill has to be dark enough for the WHITE route number
# `examples/mapbox-style.json` paints on top of it (`"text-color":
# "#ffffff"` at `text-size: 9`). `road_N` is the OPPOSITE polarity -- see
# `ROAD_PLATE_WHITE`.
NATIONAL_BLUE = (0x0B, 0x3F, 0x8F)  # 国道 -- the blue "おにぎり"
EXPRESSWAY_GREEN = (0x00, 0x70, 0x3C)  # 高速道路
PREFECTURAL_BLUE = (0x0B, 0x3F, 0x8F)  # 都道府県道 -- the blue hexagon
DEFAULT_SLATE = (0x3B, 0x43, 0x51)  # unknown network
ARROW_GREY = (0x5A, 0x64, 0x72)  # one-way arrows
WHITE = (0xFF, 0xFF, 0xFF)
MAKI_SLATE = (0x33, 0x3F, 0x4D)  # maki glyphs, recolored from black

# `road_N`: a WHITE plate with a dark keyline, which is the reverse of every
# other shield here and is not a taste call. `road_N` and `default-N` are
# names from two different style documents, and those documents paint their
# route number in opposite colours:
#
# * `default-N` is asked for by `examples/mapbox-style.json`, whose shield
#   layers set `"text-color": "#ffffff"`. White ink needs a dark plate.
# * `road_N` is asked for by `examples/liberty-style.json`, which sets NO
#   `text-color`, so the spec default `#000000` applies. Dark ink needs a
#   light plate.
#
# This sheet used to alias the two to one drawing -- `road_N` was
# `default-N`'s slate art -- which put black text on a slate plate at
# **2.1:1** contrast wherever those two styles drew a shield, below even the
# 3:1 floor WCAG allows large text. It also
# disagreed with the sheet whose names we are borrowing: OpenFreeMap's own
# `ofm@2x` (fetched into the sprite cache by the offline demo)
# draws `road_3` as a white plate with a ~1.5 sheet-px dark border, which is
# exactly why Liberty leaves its shield text black.
ROAD_PLATE_WHITE = WHITE
ROAD_PLATE_KEYLINE = DEFAULT_SLATE

# Half the keyline's width, in SHEET pixels: the drawn shape is inset by this
# much from the icon box, and the keyline straddles its edge. `road_N` uses a
# thinner one than the coloured shields -- on a white plate the keyline IS
# the shield's edge rather than a separator from the map beneath, and at
# `SHIELD_KEYLINE` it read as a frame around a label instead of a sign.
SHIELD_KEYLINE = 1.5
ROAD_PLATE_KEYLINE_HALF = 1.0
ARROW_KEYLINE = 1.0

# One digit of NotoSansJP-Bold at the 18 sheet-pixel em the style's
# `text-size: 9` works out to. Measured, not guessed -- see
# `shield_width_for`.
DIGIT_ADVANCE = 11.0
SHIELD_HEIGHT = 36  # 18 logical points, for every shield in the sheet


# --- geometry --------------------------------------------------------------
def _norm(v):
    x, y = v
    length = math.hypot(x, y)
    return (x / length, y / length) if length else (0.0, 0.0)


def round_polygon(points, radii, arc_steps=12):
    """Fillets each vertex of `points` with its own radius from `radii`.

    Returns a dense polygon approximating the rounded outline. Used instead
    of `ImageDraw.rounded_rectangle` because the shields are not all
    rectangles -- the national-route sign is an inverted triangle whose
    bottom corner needs a much larger radius than its top two.
    """
    n = len(points)
    out = []
    for i in range(n):
        prev_pt = points[(i - 1) % n]
        v = points[i]
        next_pt = points[(i + 1) % n]
        d1 = _norm((prev_pt[0] - v[0], prev_pt[1] - v[1]))
        d2 = _norm((next_pt[0] - v[0], next_pt[1] - v[1]))
        cos_theta = max(-1.0, min(1.0, d1[0] * d2[0] + d1[1] * d2[1]))
        theta = math.acos(cos_theta)
        if theta < 1e-6 or abs(theta - math.pi) < 1e-6:
            out.append(v)
            continue
        radius = radii[i]
        # A fillet cannot eat more than half of either adjacent edge.
        max_tangent = min(
            math.hypot(prev_pt[0] - v[0], prev_pt[1] - v[1]),
            math.hypot(next_pt[0] - v[0], next_pt[1] - v[1]),
        ) / 2.0
        tangent = radius / math.tan(theta / 2.0)
        if tangent > max_tangent:
            tangent = max_tangent
            radius = tangent * math.tan(theta / 2.0)
        if radius <= 0.01:
            out.append(v)
            continue
        t1 = (v[0] + d1[0] * tangent, v[1] + d1[1] * tangent)
        t2 = (v[0] + d2[0] * tangent, v[1] + d2[1] * tangent)
        bisector = _norm((d1[0] + d2[0], d1[1] + d2[1]))
        centre_dist = radius / math.sin(theta / 2.0)
        cx = v[0] + bisector[0] * centre_dist
        cy = v[1] + bisector[1] * centre_dist
        a1 = math.atan2(t1[1] - cy, t1[0] - cx)
        a2 = math.atan2(t2[1] - cy, t2[0] - cx)
        # Sweep the short way round, which is always the fillet's arc.
        sweep = (a2 - a1) % (2 * math.pi)
        if sweep > math.pi:
            sweep -= 2 * math.pi
        for step in range(arc_steps + 1):
            a = a1 + sweep * step / arc_steps
            out.append((cx + radius * math.cos(a), cy + radius * math.sin(a)))
    return out


def draw_shape(size, points, fill, keyline, keyline_color=WHITE):
    """Renders one filled polygon with a white keyline straddling its edge.

    `points` are SHEET-pixel coordinates of the coloured region's outer
    boundary; the keyline extends `keyline` px outside it and `keyline` px
    inside, so the icon's silhouette is `points` grown by `keyline`.

    The RGB plane is painted on a keyline-coloured background and the alpha
    plane is built separately, so the box downsample never averages a
    coloured pixel against an undefined transparent one -- that is the dark
    fringe an RGBA downsample of an unpremultiplied image produces.
    """
    w, h = size
    scaled = [(x * SS, y * SS) for x, y in points]
    closed = scaled + [scaled[0]]
    stroke = max(1, int(round(2 * keyline * SS)))

    mask = Image.new("L", (w * SS, h * SS), 0)
    md = ImageDraw.Draw(mask)
    md.polygon(scaled, fill=255)
    md.line(closed, fill=255, width=stroke, joint="curve")

    rgb = Image.new("RGB", (w * SS, h * SS), keyline_color)
    rd = ImageDraw.Draw(rgb)
    rd.polygon(scaled, fill=fill)
    rd.line(closed, fill=keyline_color, width=stroke, joint="curve")

    rgb = rgb.resize((w, h), Image.Resampling.BOX)
    mask = mask.resize((w, h), Image.Resampling.BOX)
    out = rgb.convert("RGBA")
    out.putalpha(mask)
    return out


# --- shield shapes ---------------------------------------------------------
def rounded_rect_shield(w, h, fill):
    """高速道路 / fallback: a rounded rectangle. Widest at its centre, so a
    centred number has the whole box to sit in."""
    m = SHIELD_KEYLINE
    pts = [(m, m), (w - m, m), (w - m, h - m), (m, h - m)]
    radius = 5.0
    return draw_shape((w, h), round_polygon(pts, [radius] * 4), fill, SHIELD_KEYLINE)


def road_plate(w, h):
    """`road_N`: the light plate Liberty paints DARK text on.

    Same silhouette as `rounded_rect_shield`, inverted: a white fill with a
    slate keyline rather than a slate fill with a white one. See
    `ROAD_PLATE_WHITE` for why this is not the same drawing as `default-N`.
    """
    m = ROAD_PLATE_KEYLINE_HALF
    pts = [(m, m), (w - m, m), (w - m, h - m), (m, h - m)]
    radius = 5.0
    return draw_shape(
        (w, h),
        round_polygon(pts, [radius] * 4),
        ROAD_PLATE_WHITE,
        ROAD_PLATE_KEYLINE_HALF,
        keyline_color=ROAD_PLATE_KEYLINE,
    )


def onigiri_shield(w, h, fill):
    """国道: the blue inverted triangle ("おにぎり").

    The bottom is deliberately blunt -- a true triangle puts the icon's
    geometric centre, which is exactly where the style centres the route
    number, in the narrow tip. The bottom edge is 30% of the width and
    carries a large fillet, which keeps the sign readable as an inverted
    triangle while leaving the middle band wide enough for three digits.
    """
    m = SHIELD_KEYLINE
    half_bottom = 0.32 * w
    pts = [
        (m, m),
        (w - m, m),
        (w / 2.0 + half_bottom, h - m),
        (w / 2.0 - half_bottom, h - m),
    ]
    # The bottom fillet is deliberately larger than the bottom edge's own
    # half-length; `round_polygon` clamps it, and the clamp is what turns
    # the blunt end into the near-continuous curve the real sign has.
    return draw_shape((w, h), round_polygon(pts, [5.0, 5.0, 12.0, 12.0]), fill, SHIELD_KEYLINE)


def hexagon_shield(w, h, fill):
    """都道府県道: the six-sided prefectural marker, points left and right.

    Its widest band is the vertical centre, which is where the number goes.
    The shoulders are only 20% in from each end: a rounder hexagon narrows
    too fast, and the number's cap band -- 14 sheet px of a 36 px icon --
    reaches well above and below the centre line.
    """
    m = SHIELD_KEYLINE
    shoulder = 0.20 * w
    pts = [
        (m, h / 2.0),
        (shoulder, m),
        (w - shoulder, m),
        (w - m, h / 2.0),
        (w - shoulder, h - m),
        (shoulder, h - m),
    ]
    return draw_shape((w, h), round_polygon(pts, [3.0] * 6), fill, SHIELD_KEYLINE)


def oneway_arrow(size_logical):
    """A one-way arrow pointing along +X, dark grey with a thin white
    keyline so it reads over both the road fill and its casing."""
    w = h = size_logical * PIXEL_RATIO
    # A fixed keyline is half the 8-point arrow and a tenth of the 12-point
    # one; scaling it keeps both looking like the same drawing.
    keyline = max(0.75, size_logical * ARROW_KEYLINE / 12.0)
    m = keyline
    # The keyline eats `ARROW_KEYLINE` off every edge, so a shaft thin
    # enough to look right on paper comes out as pure keyline at 16 sheet
    # px. These fractions are what leaves the 8-point arrow with a grey
    # shaft still 3 px thick.
    shaft_half = h * 0.17
    head_half = h * 0.34
    tip_x = w - m
    neck_x = w - m - h * 0.40
    tail_x = m
    cy = h / 2.0
    pts = [
        (tail_x, cy - shaft_half),
        (neck_x, cy - shaft_half),
        (neck_x, cy - head_half),
        (tip_x, cy),
        (neck_x, cy + head_half),
        (neck_x, cy + shaft_half),
        (tail_x, cy + shaft_half),
    ]
    radii = [0.8, 0.8, 1.0, 1.2, 1.0, 0.8, 0.8]
    return draw_shape((w, h), round_polygon(pts, radii, arc_steps=6), ARROW_GREY, keyline)


def black_circle(diameter_logical):
    """`circle_11_black`: a solid black dot with a thin white ring, the
    city/town/village dot the second style asks for."""
    d = diameter_logical * PIXEL_RATIO
    m = 1.25
    r = d / 2.0 - m
    cx = cy = d / 2.0
    pts = [
        (cx + r * math.cos(2 * math.pi * i / 64), cy + r * math.sin(2 * math.pi * i / 64))
        for i in range(64)
    ]
    return draw_shape((d, d), pts, (0, 0, 0), m)


def shield_width_for(chars):
    """The sheet width of a rectangular shield holding `chars` characters.

    `11 * chars` is the measured advance of that many bold digits at the
    18 sheet-pixel em `text-size: 9` becomes at `pixelRatio: 2`; the +18 is
    the keyline plus about 4 logical points of side padding. Rounded up to
    an even number so the LOGICAL size stays a whole point.
    """
    w = int(round(DIGIT_ADVANCE * chars + 18))
    return w + (w % 2)


# --- maki ------------------------------------------------------------------
MAKI_SIZE = 15 * PIXEL_RATIO  # 30x30 sheet px == 15x15 logical


def rasterize_maki(tmpdir):
    """Rasterizes every vendored maki SVG to 30x30 and recolours it.

    maki ships monochrome black paths; only the ALPHA carries the shape, so
    replacing RGB wholesale (and keeping alpha) tints the glyph without
    touching its antialiasing.
    """
    rsvg = shutil.which("rsvg-convert") or "/opt/homebrew/bin/rsvg-convert"
    if not os.path.exists(rsvg):
        sys.exit("rsvg-convert not found; install librsvg (brew install librsvg)")
    icons = {}
    names = sorted(f[:-4] for f in os.listdir(MAKI_DIR) if f.endswith(".svg"))
    for name in names:
        src = os.path.join(MAKI_DIR, name + ".svg")
        dst = os.path.join(tmpdir, name + ".png")
        subprocess.run(
            [rsvg, "-w", str(MAKI_SIZE), "-h", str(MAKI_SIZE), src, "-o", dst],
            check=True,
        )
        img = Image.open(dst).convert("RGBA")
        alpha = img.getchannel("A")
        tinted = Image.new("RGBA", img.size, MAKI_SLATE + (255,))
        tinted.putalpha(alpha)
        icons[name] = tinted
    return icons


# OpenMapTiles class values with no maki icon of the same spelling. The
# style documents select icons with `["get", "class"]`, so these have to
# resolve or the POI draws nothing.
CLASS_ALIASES = {
    "sports_centre": "pitch",
    "ice_rink": "pitch",
    "swimming_pool": "swimming",
    "ferry_terminal": "ferry",
    "office": "suitcase",
    "railway": "rail",
    "railway_metro": "rail-metro",
    "subway": "rail-metro",
    # Three more the styles can reach through their `["get", "class"]`
    # fallback and maki spells differently.
    "pub": "beer",
    "bicycle_rental": "bicycle-share",
    "bus_stop": "bus",
}


# --- packing ---------------------------------------------------------------
def pack(images):
    """Shelf-packs `{name: image}` into one sheet, deterministically.

    Names are sorted, then stably re-sorted by descending height so equal-
    height icons share a shelf and the sheet stays compact. Both passes are
    pure functions of the name set, so a re-run lays out the same sheet.
    """
    order = sorted(images)
    order.sort(key=lambda n: -images[n].height)

    placements = {}
    x = y = shelf_h = 0
    used_w = 0
    for name in order:
        img = images[name]
        if x and x + img.width > MAX_SHEET_WIDTH:
            y += shelf_h + GUTTER
            x = 0
            shelf_h = 0
        placements[name] = (x, y)
        used_w = max(used_w, x + img.width)
        shelf_h = max(shelf_h, img.height)
        x += img.width + GUTTER
    sheet_w, sheet_h = used_w, y + shelf_h

    sheet = Image.new("RGBA", (sheet_w, sheet_h), (0, 0, 0, 0))
    manifest = {}
    for name in order:
        img = images[name]
        px, py = placements[name]
        sheet.paste(img, (px, py))
        manifest[name] = {
            "x": px,
            "y": py,
            "width": img.width,
            "height": img.height,
            "pixelRatio": PIXEL_RATIO,
            "sdf": False,
        }
    return sheet, manifest


# --- the icon set ----------------------------------------------------------
def build_icons():
    # Kept separate from the maki set until the very end, and applied ON TOP
    # of it: maki ships an `arrow.svg` of its own, and a plain
    # `images.update(maki)` silently replaced this sheet's keylined one-way
    # arrow with maki's bare 15x15 chevron. A style this sheet serves draws
    # `"arrow"` along every one-way road, so that was 300-odd road arrows
    # rendering as the wrong icon, at the wrong size, with no keyline.
    drawn = {}
    images = drawn

    # Part A: Japanese route shields, drawn here.
    for chars, name in ((2, "jp-national-route-2"), (3, "jp-national-route-3")):
        # The おにぎり tapers, so it needs more width than a rectangle for
        # the same number: the band that has to hold the digits is not the
        # icon's full width but its width at the BOTTOM of the cap band,
        # 7 sheet px below centre, where the sign has already narrowed.
        # +8 is what the preview showed leaves ~4 sheet px of blue either
        # side of "246" -- at +4 it was 1.5, and the outer digits grazed
        # the slope.
        images[name] = onigiri_shield(shield_width_for(chars) + 8, SHIELD_HEIGHT, NATIONAL_BLUE)
    for chars, name in ((2, "jp-expressway-2"), (3, "jp-expressway-3")):
        # Expressway refs are alphanumeric ("E1", "E1A"), and a capital is
        # wider than a digit, hence the extra couple of pixels.
        images[name] = rounded_rect_shield(
            shield_width_for(chars) + 4, SHIELD_HEIGHT, EXPRESSWAY_GREEN
        )
    for chars, name in ((2, "jp-prefectural-road-2"), (3, "jp-prefectural-road-3")):
        # The hexagon tapers too, just less than the triangle: at the top
        # of the digits' cap band it is already well in from each point.
        images[name] = hexagon_shield(shield_width_for(chars) + 4, SHIELD_HEIGHT, PREFECTURAL_BLUE)

    # `default-N` is the fallback badge for the style that paints WHITE
    # route numbers; `road_N` is a different style's badge for the same job
    # under DARK route numbers, so it is the same silhouette at the same
    # width and the opposite polarity. Same widths matter beyond looks: the
    # shelf packer lays out by size, so keeping `road_N` exactly as wide as
    # `default-N` leaves every rect in the manifest where it was.
    for chars in range(1, 7):
        images[f"default-{chars}"] = rounded_rect_shield(
            shield_width_for(chars), SHIELD_HEIGHT, DEFAULT_SLATE
        )
        images[f"road_{chars}"] = road_plate(shield_width_for(chars), SHIELD_HEIGHT)

    images["oneway-small"] = oneway_arrow(8)
    images["oneway-large"] = oneway_arrow(12)
    images["arrow"] = images["oneway-small"].copy()
    images["circle_11_black"] = black_circle(11)

    # Part B: maki, CC0. Our own art wins every name collision.
    with tempfile.TemporaryDirectory() as tmp:
        maki = rasterize_maki(tmp)
    images = dict(maki)
    images.update(drawn)

    # OpenMapTiles spells its class values with underscores where maki uses
    # hyphens (`fast_food` vs `fast-food`). The engine packs per NAME, so an
    # alias needs its own rect rather than a reference to another one.
    for name, img in sorted(maki.items()):
        if "-" in name:
            images.setdefault(name.replace("-", "_"), img.copy())
    for alias, target in sorted(CLASS_ALIASES.items()):
        if target not in maki:
            sys.exit(f"alias {alias} -> {target}: no such maki icon")
        images.setdefault(alias, maki[target].copy())

    return images


# --- preview (a review aid; it does not ship) ------------------------------
SAMPLE_NUMBERS = {
    "jp-national-route-2": "17",
    "jp-national-route-3": "246",
    "jp-expressway-2": "E1",
    "jp-expressway-3": "E1A",
    "jp-prefectural-road-2": "45",
    "jp-prefectural-road-3": "123",
    "default-1": "8",
    "default-2": "17",
    "default-3": "246",
    "default-4": "1234",
    "default-5": "12345",
    "default-6": "123456",
    "road_1": "8",
    "road_2": "17",
    "road_3": "246",
    "road_4": "1234",
    "road_5": "12345",
    "road_6": "123456",
}

# Extra preview tiles: the same shield with a different sample, where one
# sample cannot show everything. `NotoSansJP-Bold.japan-subset.otf` carries
# NO Latin capitals (see `japan-codepoints.txt`), so "E1" renders as a blank
# `.notdef` advance followed by a "1" -- worth SEEING in the preview, but it
# leaves the expressway badge's real width unjudged without a digit sample.
EXTRA_SAMPLES = [
    ("jp-expressway-2", "20"),
    ("jp-expressway-3", "302"),
]


def label_font():
    """A full-Latin face for the preview's own captions.

    Not the repo's subset font: that one has no `d`, `g`, `b` or any
    capital, so every caption came out with holes in it.
    """
    for path in (
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ):
        if os.path.exists(path):
            return ImageFont.truetype(path, 11)
    return ImageFont.load_default()


def label_shield(img, text, font):
    """Draws `text` on a copy of `img` the way the style asks for it: bold
    white, 9 logical px (18 sheet px), centred on the icon with no offset.

    The digits' INK box is what gets centred, not the font's line box: a
    run of digits uses none of the ascender's headroom or the descender's
    depth, so centring the line box instead would sit the number low.
    """
    out = img.copy()
    d = ImageDraw.Draw(out)
    left, top, right, bottom = font.getbbox(text)
    x = (out.width - (right - left)) / 2.0 - left
    y = (out.height - (bottom - top)) / 2.0 - top
    d.text((x, y), text, font=font, fill=(255, 255, 255, 255))
    return out


def build_preview(images):
    font = ImageFont.truetype(BOLD_FONT, 9 * PIXEL_RATIO)
    small = label_font()
    grey = (0x80, 0x80, 0x80, 0xFF)

    samples = sorted((n, SAMPLE_NUMBERS[n]) for n in SAMPLE_NUMBERS if n in images)
    samples += [(n, t) for n, t in EXTRA_SAMPLES if n in images]
    drawn = ["arrow", "circle_11_black", "oneway-large", "oneway-small"]

    # The shields at 3x nearest-neighbour, which is the only honest way to
    # judge whether a 9pt number fits: it magnifies the exact pixels a 2x
    # display shows without inventing any.
    zoom = 3
    tiles = []
    for name, text in samples:
        art = label_shield(images[name], text, font)
        tiles.append(
            (f"{name}  {text}", art.resize((art.width * zoom, art.height * zoom), Image.Resampling.NEAREST))
        )
    for name in drawn:
        art = images[name]
        tiles.append((name, art.resize((art.width * zoom, art.height * zoom), Image.Resampling.NEAREST)))

    pad = 10
    label_h = 14
    cols = 6
    cell_w = max(t.width for _, t in tiles) + pad
    cell_h = max(t.height for _, t in tiles) + pad + label_h
    rows = (len(tiles) + cols - 1) // cols
    top_h = rows * cell_h + pad

    # Then everything at its true sheet size, in one contact sheet.
    all_names = sorted(images)
    thumb_w = max(images[n].width for n in all_names) + 6
    thumb_h = max(images[n].height for n in all_names) + 6 + label_h
    tcols = max(1, 1100 // thumb_w)
    trows = (len(all_names) + tcols - 1) // tcols

    width = max(cols * cell_w + pad, tcols * thumb_w + pad)
    height = top_h + 24 + trows * thumb_h + pad
    canvas = Image.new("RGBA", (width, height), grey)
    d = ImageDraw.Draw(canvas)

    for i, (name, art) in enumerate(tiles):
        cx = pad + (i % cols) * cell_w
        cy = pad + (i // cols) * cell_h
        canvas.alpha_composite(art, (cx, cy))
        d.text((cx, cy + art.height + 2), name, font=small, fill=(0x18, 0x18, 0x18, 0xFF))

    y0 = top_h + 8
    d.text((pad, y0), "every icon at its true sheet size (2x)", font=small, fill=(0x18, 0x18, 0x18, 0xFF))
    y0 += 16
    for i, name in enumerate(all_names):
        art = images[name]
        cx = pad + (i % tcols) * thumb_w
        cy = y0 + (i // tcols) * thumb_h
        canvas.alpha_composite(art, (cx, cy))
        d.text((cx, cy + art.height + 1), name[:14], font=small, fill=(0x22, 0x22, 0x22, 0xFF))

    canvas.convert("RGB").save(PREVIEW_PNG, optimize=True)


def main():
    images = build_icons()
    sheet, manifest = pack(images)
    sheet.save(SHEET_PNG, optimize=True)
    with open(SHEET_JSON, "w") as f:
        json.dump(manifest, f, indent=1, sort_keys=True)
        f.write("\n")
    build_preview(images)
    print(f"{len(manifest)} entries, sheet {sheet.width}x{sheet.height}")
    print(f"  {SHEET_PNG}  {os.path.getsize(SHEET_PNG):,} bytes")
    print(f"  {SHEET_JSON}  {os.path.getsize(SHEET_JSON):,} bytes")
    print(f"  {PREVIEW_PNG}  {os.path.getsize(PREVIEW_PNG):,} bytes")


if __name__ == "__main__":
    main()

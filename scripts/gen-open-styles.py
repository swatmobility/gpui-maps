#!/usr/bin/env python3
"""Generates the open look-alike basemap styles in examples/styles-open/.

Each output is OpenFreeMap Liberty (examples/liberty-style.json: code
BSD-3-Clause, design CC BY 4.0; a fork of maputnik/osm-liberty) with OUR OWN
paint and layout changes applied by layer id, over the same OpenFreeMap /
OpenMapTiles planet source, OpenFreeMap glyphs (Noto Sans, OFL) and the
OpenFreeMap sprite. No Mapbox style document, layer list or sprite is read
or copied here: the colours were picked by eye to resemble each Mapbox
look, and the layer structure is Liberty's own.

Run from the repo root:  python3 -I scripts/gen-open-styles.py
The outputs are committed; a test checks they stay free of Mapbox hosts.
"""

import colorsys
import copy
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASE = ROOT / "examples" / "liberty-style.json"
OUT = ROOT / "examples" / "styles-open"

PROVENANCE = (
    "Based on OpenFreeMap Liberty (https://github.com/hyperknot/openfreemap-styles; "
    "code BSD-3-Clause, design CC BY 4.0; a fork of maputnik/osm-liberty). "
    "Colours, widths and label density are gpui-map's own modifications "
    "(scripts/gen-open-styles.py). Tiles: OpenFreeMap, OpenMapTiles, OpenStreetMap contributors (ODbL). "
    "Glyphs: Noto Sans (OFL) served by OpenFreeMap. Sprite: OpenFreeMap."
)

# ---------------------------------------------------------------- colours

HEX = re.compile(r"^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$")
FUNC = re.compile(r"^(rgba?|hsla?)\(([^)]*)\)$")


def parse_colour(s):
    """(r, g, b, a) in 0..1, or None when `s` is not a colour."""
    if not isinstance(s, str):
        return None
    s = s.strip()
    m = HEX.match(s)
    if m:
        h = m.group(1)
        if len(h) == 3:
            h = "".join(c * 2 for c in h)
        return (int(h[0:2], 16) / 255, int(h[2:4], 16) / 255, int(h[4:6], 16) / 255, 1.0)
    m = FUNC.match(s)
    if not m:
        return None
    kind, parts = m.group(1), [p.strip() for p in m.group(2).split(",")]
    a = float(parts[3]) if len(parts) > 3 else 1.0
    if kind.startswith("rgb"):
        return (float(parts[0]) / 255, float(parts[1]) / 255, float(parts[2]) / 255, a)
    h = float(parts[0]) / 360
    sat = float(parts[1].rstrip("%")) / 100
    lig = float(parts[2].rstrip("%")) / 100
    r, g, b = colorsys.hls_to_rgb(h, lig, sat)
    return (r, g, b, a)


def fmt(r, g, b, a=1.0):
    r, g, b = (max(0, min(255, round(c * 255))) for c in (r, g, b))
    if a >= 0.999:
        return f"#{r:02x}{g:02x}{b:02x}"
    return f"rgba({r},{g},{b},{round(a, 3)})"


def map_colours(value, fn):
    """`value` (a paint value, possibly an expression) with every colour
    string replaced by fn(r, g, b, a)."""
    if isinstance(value, list):
        return [map_colours(v, fn) for v in value]
    c = parse_colour(value)
    return fn(*c) if c else value


def night(hue, sat, l_lo, l_hi):
    """A colour transform for a dark style: lightness inverted into
    [l_lo, l_hi], tinted toward `hue` at saturation `sat`."""

    def f(r, g, b, a):
        h, l, s = colorsys.rgb_to_hls(r, g, b)
        l2 = l_lo + (1 - l) * (l_hi - l_lo)
        r2, g2, b2 = colorsys.hls_to_rgb(hue / 360, l2, sat)
        return fmt(r2, g2, b2, a)

    return f


# ---------------------------------------------------------------- edits


def layer_map(style):
    return {l["id"]: l for l in style["layers"]}


def paint(style, ids, **props):
    """Sets paint properties (underscores -> dashes) on every id in `ids`
    that exists (a prefix ending in '*' matches many)."""
    layers = style["layers"]
    for pat in ids:
        hits = [l for l in layers if (l["id"].startswith(pat[:-1]) if pat.endswith("*") else l["id"] == pat)]
        if not hits:
            sys.exit(f"no layer matches {pat!r}")
        for l in hits:
            p = l.setdefault("paint", {})
            for k, v in props.items():
                p[k.replace("_", "-")] = v


def layout(style, ids, **props):
    lm = layer_map(style)
    for i in ids:
        lo = lm[i].setdefault("layout", {})
        for k, v in props.items():
            lo[k.replace("_", "-")] = v


def drop(style, ids):
    style["layers"] = [l for l in style["layers"] if l["id"] not in ids]


def symbols(style):
    return [l["id"] for l in style["layers"] if l["type"] == "symbol"]


def base(name):
    s = json.loads(BASE.read_text())
    s["name"] = name
    s.setdefault("metadata", {})
    s["metadata"] = {"gpui-map:provenance": PROVENANCE}
    return s


# ---------------------------------------------------------------- open-streets


def open_streets():
    """Light and warm: cream ground, teal-blue water, saturated parks,
    orange motorways, yellow trunks/primaries, white streets with grey
    casings, dark grey labels."""
    s = base("Streets (open)")
    paint(s, ["background"], background_color="#f6f2ec")
    paint(s, ["water"], fill_color="#7ccdf0")
    paint(s, ["waterway_river", "waterway_other", "waterway_tunnel"], line_color="#7ccdf0")
    paint(s, ["park"], fill_color="#c4e8a6", fill_outline_color="rgba(150,205,110,0.6)")
    paint(s, ["park_outline"], line_color="rgba(196,232,166,0.8)")
    paint(s, ["landcover_wood"], fill_color="rgba(170,218,140,0.55)")
    paint(s, ["landcover_grass"], fill_color="#cdebb4")
    paint(s, ["landuse_residential"], fill_color="rgba(238,232,224,0.6)")
    paint(s, ["landuse_hospital"], fill_color="#f6dfe3")
    paint(s, ["landuse_school"], fill_color="#efe7d6")
    paint(s, ["landuse_cemetery"], fill_color="#d6e6c6")
    paint(s, ["landuse_pitch", "landuse_track"], fill_color="#cfe9b8")
    paint(s, ["aeroway_fill"], fill_color="#e8e4ee")
    paint(s, ["building"], fill_color="#e6ded4", fill_outline_color="#d8cfc4")
    paint(s, ["building-3d"], fill_extrusion_color="#e6ded4")
    # roads: motorway orange, trunk/primary yellow, secondary/tertiary pale
    # yellow, the rest white over a grey casing
    motorway, motorway_case = "#ffa04f", "#e2803a"
    trunk, trunk_case = "#ffd25e", "#e0b046"
    sec, sec_case = "#fff3b0", "#dccf9a"
    street_case = "#d6d1ca"
    for brunnel in ("road", "bridge", "tunnel"):
        paint(s, [f"{brunnel}_motorway", f"{brunnel}_motorway_link"], line_color=motorway)
        paint(s, [f"{brunnel}_motorway_casing", f"{brunnel}_motorway_link_casing"], line_color=motorway_case)
        paint(s, [f"{brunnel}_trunk_primary"], line_color=trunk)
        paint(s, [f"{brunnel}_trunk_primary_casing"], line_color=trunk_case)
        paint(s, [f"{brunnel}_secondary_tertiary", f"{brunnel}_link"], line_color=sec)
        paint(s, [f"{brunnel}_secondary_tertiary_casing", f"{brunnel}_link_casing"], line_color=sec_case)
        paint(s, [f"{brunnel}_service_track_casing"], line_color=street_case)
    paint(s, ["road_minor_casing", "tunnel_street_casing", "bridge_street_casing"], line_color=street_case)
    paint(s, ["road_major_rail", "road_transit_rail", "bridge_major_rail", "bridge_transit_rail"], line_color="#c4bfc8")
    paint(s, ["boundary_2", "boundary_disputed"], line_color="#a49bb0")
    paint(s, ["boundary_3"], line_color="#c8bfd0")
    # labels: dark grey on a white halo; POIs a muted brown-grey, transit blue
    for i in ("label_city", "label_city_capital", "label_town", "label_village", "label_country_1", "label_country_2", "label_country_3"):
        paint(s, [i], text_color="#3d3a37", text_halo_color="rgba(255,255,255,0.9)")
    paint(s, ["label_other", "label_state"], text_color="#6b6560", text_halo_color="rgba(255,255,255,0.85)")
    paint(s, ["highway-name-minor", "highway-name-major"], text_color="#57524d", text_halo_color="#ffffff", text_halo_width=1.2)
    paint(s, ["poi_r1", "poi_r7", "poi_r20"], text_color="#79695c", text_halo_color="#ffffff")
    paint(s, ["poi_transit"], text_color="#3a68b4", text_halo_color="#ffffff")
    paint(s, ["water_name_point_label", "water_name_line_label", "waterway_line_label"], text_color="#2f7ea6", text_halo_color="rgba(255,255,255,0.7)")
    return s


# ---------------------------------------------------------------- dark looks


def darken(s, hue, sat, l_lo, l_hi):
    for l in s["layers"]:
        if "paint" in l:
            l["paint"] = {k: (map_colours(v, night(hue, sat, l_lo, l_hi)) if "color" in k else v) for k, v in l["paint"].items()}
    drop(s, {"natural_earth"})  # the light shaded relief reads as fog on a dark ground


def open_dark():
    """Night monochrome: a neutral dark grey ground, darker blue-grey water,
    lighter grey roads, quiet grey admin lines, muted place labels; no
    POIs and no road names (that look hides them)."""
    s = base("Dark (open)")
    darken(s, 220, 0.04, 0.09, 0.42)
    paint(s, ["background"], background_color="#1c1d21")
    paint(s, ["water"], fill_color="#2c3038")
    paint(s, ["waterway_river", "waterway_other", "waterway_tunnel"], line_color="#2c3038")
    paint(s, ["park", "landcover_wood", "landcover_grass", "landuse_cemetery", "landuse_pitch", "landuse_track"], fill_color="#22252a")
    paint(s, ["park"], fill_outline_color="rgba(0,0,0,0)")
    paint(s, ["park_outline"], line_color="rgba(0,0,0,0)")
    paint(s, ["landuse_residential", "landuse_school", "landuse_hospital", "aeroway_fill", "landcover_sand", "landcover_ice"], fill_color="#202125")
    paint(s, ["building"], fill_color="#26282d", fill_outline_color="#2e3036")
    paint(s, ["building-3d"], fill_extrusion_color="#2a2c31")
    for brunnel in ("road", "bridge", "tunnel"):
        paint(s, [f"{brunnel}_motorway", f"{brunnel}_motorway_link", f"{brunnel}_trunk_primary"], line_color="#5d6068")
        paint(s, [f"{brunnel}_secondary_tertiary", f"{brunnel}_link"], line_color="#4b4e55")
        paint(s, [f"{brunnel}_service_track"], line_color="#36383d")
        paint(s, [f"{brunnel}_motorway_casing", f"{brunnel}_motorway_link_casing", f"{brunnel}_trunk_primary_casing",
                  f"{brunnel}_secondary_tertiary_casing", f"{brunnel}_link_casing", f"{brunnel}_service_track_casing"], line_color="#1c1d21")
    paint(s, ["road_minor", "tunnel_minor", "bridge_street"], line_color="#3f4147")
    paint(s, ["road_minor_casing", "tunnel_street_casing", "bridge_street_casing"], line_color="#1c1d21")
    paint(s, ["road_path_pedestrian", "tunnel_path_pedestrian", "bridge_path_pedestrian"], line_color="#33353a")
    # country lines a quiet grey a few steps above the ground (as Standard
    # Dark and Dark Matter draw them): visible, but under data and labels;
    # the subnational lines (admin 3-8, which in Singapore are planning
    # areas) stay quieter still
    paint(s, ["boundary_2", "boundary_disputed"], line_color="#575a62")
    paint(s, ["boundary_3"], line_color="#34363c")
    for i in symbols(s):
        paint(s, [i], text_color="#9a9ca3", text_halo_color="#16171a")
    paint(s, ["label_city", "label_city_capital", "label_country_1", "label_country_2", "label_country_3"], text_color="#c4c6cc")
    paint(s, ["water_name_point_label", "water_name_line_label", "waterway_line_label"], text_color="#7e8899")
    drop(s, {"poi_r1", "poi_r7", "poi_r20", "poi_transit", "highway-name-path", "highway-name-minor", "highway-name-major",
             "road_one_way_arrow", "road_one_way_arrow_opposite",
             "highway-shield-non-us", "highway-shield-us-interstate", "road_shield_us"})
    return s


def open_swat_routes_dark():
    """A quiet backdrop for routes: slate-navy ground, near-black water,
    low-contrast grey-blue roads with no casings, muted blue-grey labels,
    sparse POIs. Nothing on it competes with a coloured route line."""
    s = base("Graphite")
    darken(s, 222, 0.12, 0.10, 0.36)
    paint(s, ["background"], background_color="#1b1f26")
    paint(s, ["water"], fill_color="#08090b")
    paint(s, ["waterway_river", "waterway_other", "waterway_tunnel"], line_color="#08090b")
    paint(s, ["park", "landcover_wood", "landcover_grass", "landuse_cemetery", "landuse_pitch", "landuse_track"], fill_color="#1f242b")
    paint(s, ["park"], fill_outline_color="rgba(0,0,0,0)")
    paint(s, ["park_outline"], line_color="rgba(0,0,0,0)")
    paint(s, ["landuse_residential", "landuse_school", "landuse_hospital", "landcover_sand", "landcover_ice"], fill_color="#1d2128")
    paint(s, ["aeroway_fill"], fill_color="#23272f")
    paint(s, ["building"], fill_color="#23272f", fill_outline_color="rgba(0,0,0,0.6)")
    paint(s, ["building-3d"], fill_extrusion_color="#262a33")
    for brunnel in ("road", "bridge", "tunnel"):
        paint(s, [f"{brunnel}_motorway", f"{brunnel}_motorway_link", f"{brunnel}_trunk_primary"], line_color="#3a3e4a")
        paint(s, [f"{brunnel}_secondary_tertiary", f"{brunnel}_link"], line_color="#343844")
        paint(s, [f"{brunnel}_service_track"], line_color="#2b2f38")
        paint(s, [f"{brunnel}_motorway_casing", f"{brunnel}_motorway_link_casing", f"{brunnel}_trunk_primary_casing",
                  f"{brunnel}_secondary_tertiary_casing", f"{brunnel}_link_casing", f"{brunnel}_service_track_casing"], line_color="#1b1f26")
    paint(s, ["road_minor", "tunnel_minor", "bridge_street"], line_color="#2e323c")
    paint(s, ["road_minor_casing", "tunnel_street_casing", "bridge_street_casing"], line_color="#1b1f26")
    paint(s, ["road_path_pedestrian", "tunnel_path_pedestrian", "bridge_path_pedestrian"], line_color="#2a2d35")
    paint(s, ["road_major_rail", "road_transit_rail", "bridge_major_rail", "bridge_transit_rail",
              "tunnel_major_rail", "tunnel_transit_rail"], line_color="#24262a")
    paint(s, ["boundary_2", "boundary_disputed", "boundary_3"], line_color="#3e4559")
    for i in symbols(s):
        paint(s, [i], text_color="#7f8292", text_halo_color="#070708")
    paint(s, ["label_city", "label_city_capital", "label_town", "highway-name-major", "highway-name-minor", "airport"], text_color="#a3a5b1")
    paint(s, ["label_country_1", "label_country_2", "label_country_3"], text_color="#5f616f")
    paint(s, ["water_name_point_label", "water_name_line_label", "waterway_line_label"], text_color="#6d7385")
    paint(s, ["poi_r1"], icon_opacity=0.35)
    for l in s["layers"]:
        if l["id"] == "poi_r1":
            l["minzoom"] = 16  # sparse: only the most important POIs, and late
    drop(s, {"poi_r7", "poi_r20", "poi_transit", "road_one_way_arrow", "road_one_way_arrow_opposite",
             "highway-shield-non-us", "highway-shield-us-interstate", "road_shield_us",
             "road_major_rail_hatching", "road_transit_rail_hatching", "bridge_major_rail_hatching",
             "bridge_transit_rail_hatching", "tunnel_major_rail_hatching", "tunnel_transit_rail_hatching"})
    return s


# ---------------------------------------------------------------- light looks


def pale(hue, sat, l_lo, l_hi):
    """A colour transform for a light style: lightness squeezed into
    [l_lo, l_hi], tinted toward `hue` at saturation `sat`."""

    def f(r, g, b, a):
        h, l, s = colorsys.rgb_to_hls(r, g, b)
        l2 = l_lo + l * (l_hi - l_lo)
        r2, g2, b2 = colorsys.hls_to_rgb(hue / 360, l2, sat)
        return fmt(r2, g2, b2, a)

    return f


def lighten(s, hue, sat, l_lo, l_hi):
    for l in s["layers"]:
        if "paint" in l:
            l["paint"] = {k: (map_colours(v, pale(hue, sat, l_lo, l_hi)) if "color" in k else v) for k, v in l["paint"].items()}


def calm_light(s):
    """The shared light palette: off-white ground, pale grey-blue water,
    white roads on hairline grey casings, grey labels."""
    lighten(s, 215, 0.06, 0.55, 0.97)
    paint(s, ["background"], background_color="#f3f3f1")
    paint(s, ["water"], fill_color="#d4dde5")
    paint(s, ["waterway_river", "waterway_other", "waterway_tunnel"], line_color="#d4dde5")
    paint(s, ["park", "landcover_wood", "landcover_grass", "landuse_cemetery", "landuse_pitch", "landuse_track"], fill_color="#e8ebe4")
    paint(s, ["park"], fill_outline_color="rgba(0,0,0,0)")
    paint(s, ["park_outline"], line_color="rgba(0,0,0,0)")
    paint(s, ["landuse_residential", "landuse_school", "landuse_hospital", "landcover_sand", "landcover_ice"], fill_color="#efefec")
    paint(s, ["aeroway_fill"], fill_color="#e9e9e6")
    paint(s, ["building"], fill_color="#e6e5e2", fill_outline_color="#dad9d5")
    paint(s, ["building-3d"], fill_extrusion_color="#e6e5e2")
    for brunnel in ("road", "bridge", "tunnel"):
        paint(s, [f"{brunnel}_motorway", f"{brunnel}_motorway_link", f"{brunnel}_trunk_primary",
                  f"{brunnel}_secondary_tertiary", f"{brunnel}_link", f"{brunnel}_service_track"], line_color="#ffffff")
        paint(s, [f"{brunnel}_motorway_casing", f"{brunnel}_motorway_link_casing", f"{brunnel}_trunk_primary_casing",
                  f"{brunnel}_secondary_tertiary_casing", f"{brunnel}_link_casing", f"{brunnel}_service_track_casing"], line_color="#d9d9d6")
    paint(s, ["road_minor", "tunnel_minor", "bridge_street"], line_color="#ffffff")
    paint(s, ["road_minor_casing", "tunnel_street_casing", "bridge_street_casing"], line_color="#e0e0dd")
    paint(s, ["road_major_rail", "road_transit_rail", "bridge_major_rail", "bridge_transit_rail",
              "tunnel_major_rail", "tunnel_transit_rail"], line_color="#d6d6d3")
    paint(s, ["boundary_2", "boundary_disputed"], line_color="#b4b8c0")
    paint(s, ["boundary_3"], line_color="#d0d2d6")
    for i in symbols(s):
        paint(s, [i], text_color="#7a7d84", text_halo_color="rgba(255,255,255,0.9)")
    paint(s, ["label_city", "label_city_capital", "label_country_1", "label_country_2", "label_country_3"], text_color="#55585f")
    paint(s, ["water_name_point_label", "water_name_line_label", "waterway_line_label"], text_color="#8a98a8")
    drop(s, {"poi_r1", "poi_r7", "poi_r20", "poi_transit", "road_one_way_arrow", "road_one_way_arrow_opposite",
             "highway-shield-non-us", "highway-shield-us-interstate", "road_shield_us",
             "road_major_rail_hatching", "road_transit_rail_hatching", "bridge_major_rail_hatching",
             "bridge_transit_rail_hatching", "tunnel_major_rail_hatching", "tunnel_transit_rail_hatching"})


def open_light():
    """Graphite's light twin: a calm, low-saturation base for data, no
    POIs, no shields, nothing coloured to compete with a data layer."""
    s = base("Light")
    calm_light(s)
    drop(s, {"natural_earth"})
    return s


def open_terrain():
    """The light palette over Natural Earth II shaded relief (public
    domain, OpenFreeMap's `ne2_shaded`, z0-6): the relief at full strength
    to z8 and fading out by z10, the land and park fills translucent so it
    shows through, water drawn over it."""
    s = base("Terrain")
    calm_light(s)
    for l in s["layers"]:
        if l["id"] == "natural_earth":
            l["maxzoom"] = 10
            l["paint"] = {"raster-opacity": ["interpolate", ["linear"], ["zoom"], 0, 0.95, 8, 0.85, 10, 0.0]}
    paint(s, ["background"], background_color="#eeede8")
    paint(s, ["park", "landcover_wood", "landcover_grass", "landuse_cemetery", "landuse_pitch", "landuse_track",
              "landuse_residential", "landcover_sand", "landcover_ice"], fill_opacity=0.25)
    paint(s, ["water"], fill_color="#c7d6e3")
    return s


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for name, build in [("open-streets", open_streets), ("open-dark", open_dark), ("open-swat-routes-dark", open_swat_routes_dark),
                        ("open-light", open_light), ("open-terrain", open_terrain)]:
        style = build()
        text = json.dumps(style, indent=2, ensure_ascii=False) + "\n"
        for banned in ("mapbox://", "api.mapbox.com", "tiles.mapbox.com"):
            assert banned not in text, f"{name}: {banned}"
        (OUT / f"{name}-style.json").write_text(text)
        print(f"{name}: {len(style['layers'])} layers")


if __name__ == "__main__":
    main()

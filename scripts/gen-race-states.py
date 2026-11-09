#!/usr/bin/env python3
"""Generates the race map's States mode data (demos/race_dots/states_data.rs
and demos/race_dots/states.geojson) from two public-domain Census inputs the
pipeline already has:

  - the per-state P2 rows of the 2020 P.L. 94-171 state summary (SUMLEV 040),
    as `tools/race-build build` recorded them in race-build.report.json
    (`states[].state_row`: total, then the 8 groups in walker::GROUPS order);
  - the 2020 cartographic boundary states at 1:20,000,000
    (cb_2020_us_state_20m.zip, `scripts/fetch-census-2020.sh --cb-states`).

    scripts/gen-race-states.py [DATA_DIR]

DATA_DIR defaults to ${GPUI_MAP_DATA:-~/.cache/gpui-map-data}/census2020.
Stdlib only. The boundaries are rounded to 0.001 degrees (~110 m), more
than the 1:20M source resolves, and written as one MultiLineString per state
(outlines only). Each state's pie anchor is the pole of inaccessibility of
its largest polygon, so it sits inside the state (Michigan's lower
peninsula, Hawaii's Big Island, mainland Alaska) rather than at a centroid
that may fall outside it. Fails if the 51 rows do not sum to the US total
walker::US_TOTAL holds.
"""
import heapq
import io
import json
import math
import os
import struct
import sys
import zipfile

US_TOTAL = 331_449_281
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read_dbf(data):
    n, hlen, rlen = struct.unpack("<xxxxIHH", data[:12])
    fields = []
    off = 32
    while data[off] != 0x0D:
        name = data[off:off + 11].split(b"\0")[0].decode()
        flen = data[off + 16]
        fields.append((name, flen))
        off += 32
    rows = []
    for i in range(n):
        rec = data[hlen + i * rlen + 1: hlen + (i + 1) * rlen]
        row, p = {}, 0
        for name, flen in fields:
            row[name] = rec[p:p + flen].decode("latin-1").strip()
            p += flen
        rows.append(row)
    return rows


def read_shp(data):
    shapes, off = [], 100
    while off < len(data):
        _, clen = struct.unpack(">ii", data[off:off + 8])
        body = data[off + 8: off + 8 + clen * 2]
        off += 8 + clen * 2
        stype = struct.unpack("<i", body[:4])[0]
        if stype != 5:
            raise SystemExit(f"shape type {stype}, want 5 (polygon)")
        nparts, npts = struct.unpack("<ii", body[36:44])
        parts = list(struct.unpack(f"<{nparts}i", body[44:44 + 4 * nparts]))
        p0 = 44 + 4 * nparts
        pts = [struct.unpack("<dd", body[p0 + 16 * k: p0 + 16 * k + 16]) for k in range(npts)]
        rings = [pts[parts[k]: parts[k + 1] if k + 1 < nparts else npts] for k in range(nparts)]
        shapes.append(rings)
    return shapes


def signed_area(ring):
    return sum(x0 * y1 - x1 * y0 for (x0, y0), (x1, y1) in zip(ring, ring[1:] + ring[:1])) / 2.0


def merc(p):
    lon, lat = p
    return (lon, math.degrees(math.log(math.tan(math.pi / 4 + math.radians(lat) / 2))))


def point_seg_dist2(px, py, a, b):
    (ax, ay), (bx, by) = a, b
    dx, dy = bx - ax, by - ay
    if dx or dy:
        t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
        ax, ay = ax + t * dx, ay + t * dy
    return (px - ax) ** 2 + (py - ay) ** 2


def signed_dist(px, py, ring):
    inside, best = False, math.inf
    for a, b in zip(ring, ring[1:] + ring[:1]):
        if (a[1] > py) != (b[1] > py) and px < (b[0] - a[0]) * (py - a[1]) / (b[1] - a[1]) + a[0]:
            inside = not inside
        best = min(best, point_seg_dist2(px, py, a, b))
    return (1 if inside else -1) * math.sqrt(best)


def polylabel(ring, precision=0.01):
    """Pole of inaccessibility (Mapbox's polylabel, ISC), on a Mercator ring."""
    xs, ys = [p[0] for p in ring], [p[1] for p in ring]
    x0, y0, x1, y1 = min(xs), min(ys), max(xs), max(ys)
    size = min(x1 - x0, y1 - y0)
    h = size / 2
    queue = []

    def push(x, y, h):
        d = signed_dist(x, y, ring)
        heapq.heappush(queue, (-(d + h * math.sqrt(2)), d, x, y, h))

    x = x0
    while x < x1:
        y = y0
        while y < y1:
            push(x + h, y + h, h)
            y += size
        x += size
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    best = (signed_dist(cx, cy, ring), cx, cy)
    while queue:
        _, d, x, y, h = heapq.heappop(queue)
        if d > best[0]:
            best = (d, x, y)
        if d + h * math.sqrt(2) - best[0] <= precision:
            continue
        h /= 2
        for sx in (-h, h):
            for sy in (-h, h):
                push(x + sx, y + sy, h)
    return best[1], best[2]


def unmerc(x, y):
    return x, math.degrees(math.atan(math.sinh(math.radians(y))))


def main():
    data_dir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.environ.get("GPUI_MAP_DATA", os.path.expanduser("~/.cache/gpui-map-data")), "census2020")
    report = json.load(open(os.path.join(data_dir, "race-build.report.json")))
    rows = {s["fips"]: s for s in report["states"]}
    if len(rows) != 51:
        raise SystemExit(f"{len(rows)} states in the report, want 51 (50 states and DC)")
    total = sum(s["state_row"][0] for s in rows.values())
    if total != US_TOTAL:
        raise SystemExit(f"the state rows sum to {total}, not walker::US_TOTAL {US_TOTAL}")
    for s in rows.values():
        if sum(s["state_row"][1:]) != s["state_row"][0]:
            raise SystemExit(f"{s['abbr']}: its groups do not sum to its total")

    z = zipfile.ZipFile(os.path.join(data_dir, "cb", "cb_2020_us_state_20m.zip"))
    dbf = read_dbf(z.read("cb_2020_us_state_20m.dbf"))
    shp = read_shp(z.read("cb_2020_us_state_20m.shp"))

    out_rows, features = [], []
    for rec, rings in zip(dbf, shp):
        fips = int(rec["STATEFP"])
        if fips not in rows:
            continue  # Puerto Rico: not in the dots' 50 states and DC
        s = rows.pop(fips)
        if s["abbr"].upper() != rec["STUSPS"]:
            raise SystemExit(f"FIPS {fips}: report {s['abbr']} vs boundary {rec['STUSPS']}")
        # Alaska's Aleutians cross the antimeridian: its box (the fly-to)
        # keeps to the western hemisphere's side.
        boxed = [r for r in rings if not (fips == 2 and min(p[0] for p in r) > 0)]
        lons = [p[0] for r in boxed for p in r]
        lats = [p[1] for r in boxed for p in r]
        bbox = (min(lons), min(lats), max(lons), max(lats))
        # Shapefile outer rings are clockwise (negative signed area).
        outers = [r for r in rings if signed_area(r) < 0]
        biggest = max(outers, key=lambda r: abs(signed_area([merc(p) for p in r])))
        lx, ly = unmerc(*polylabel([merc(p) for p in biggest]))
        lines = []
        for r in rings:
            line = []
            for lon, lat in r + r[:1]:
                q = [round(lon, 3), round(lat, 3)]
                if not line or line[-1] != q:
                    line.append(q)
            if len(line) >= 4:
                lines.append(line)
        features.append({"type": "Feature", "properties": {"fips": fips},
                         "geometry": {"type": "MultiLineString", "coordinates": lines}})
        out_rows.append((fips, rec["STUSPS"], rec["NAME"], s["state_row"][1:], (lx, ly), bbox))
    if rows:
        raise SystemExit(f"no boundary for FIPS {sorted(rows)}")
    out_rows.sort()
    features.sort(key=lambda f: f["properties"]["fips"])

    rs = io.StringIO()
    rs.write("// @generated by scripts/gen-race-states.py -- do not edit by hand.\n")
    rs.write("// 2020 Census P.L. 94-171 table P2 by state (SUMLEV 040), from race-build.report.json;\n")
    rs.write("// pie anchors and boxes from cb_2020_us_state_20m (U.S. Census Bureau, public domain).\n\n")
    rs.write("use super::states::StateRow;\n\n")
    rs.write(f"/// The 50 states and DC, by FIPS code.\npub const STATE_ROWS: [StateRow; {len(out_rows)}] = [\n")
    for fips, abbr, name, counts, (lx, ly), (w, s_, e, n) in out_rows:
        c = ", ".join(f"{v:_}" if v >= 10_000 else str(v) for v in counts)
        rs.write(f"    StateRow {{ fips: {fips}, abbr: \"{abbr}\", name: \"{name}\", counts: [{c}], "
                 f"anchor: ({lx:.3f}, {ly:.3f}), bbox: [{w:.3f}, {s_:.3f}, {e:.3f}, {n:.3f}] }},\n")
    rs.write("];\n")
    with open(os.path.join(ROOT, "demos/race_dots/states_data.rs"), "w") as f:
        f.write(rs.getvalue())
    gj = json.dumps({"type": "FeatureCollection", "features": features}, separators=(",", ":"))
    with open(os.path.join(ROOT, "demos/race_dots/states.geojson"), "w") as f:
        f.write(gj + "\n")
    print(f"gen-race-states: {len(out_rows)} states, {total:,} people, boundaries {len(gj):,} bytes")


if __name__ == "__main__":
    main()

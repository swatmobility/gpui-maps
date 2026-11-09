#!/usr/bin/env python3
"""Pixel diff and colour count for the RGBA8 PNGs globe_capture writes.

Standard library only (no PIL on this machine is assumed).

  png-diff.py A.png B.png [--tolerance N] [--min-differing N] [--max-differing N]
      prints `differing=<px> worst=<channel delta> pixels=<total>`;
      exits 1 when a --min/--max bound is violated.
  png-diff.py A.png B.png --mask x0,y0,x1,y1[;x0,y0,x1,y1...]
      also prints `outside=<px>`: differing pixels that lie in none of the
      rects (pixel coordinates, half-open); exits 1 when there is any.
  png-diff.py --count R,G,B,A A.png [--rows FROM,TO]
      prints `matching=<px>` for pixels exactly equal to the colour,
      optionally only in pixel rows FROM..TO (half-open, 0 = top).
  png-diff.py --count-new R,G,B,A A.png B.png [--rows FROM,TO]
      prints `new=<px>` for pixels exactly equal to the colour in A but
      not in B: what A's extra layer uncovered (a hole), not what B never
      painted either.
"""
import struct, sys, zlib


def read_png(path):
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        sys.exit(f"{path}: not a PNG")
    pos, idat = 8, []
    w = h = bpp = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        kind, body = data[pos + 4:pos + 8], data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            if depth != 8 or ctype not in (2, 6) or interlace:
                sys.exit(f"{path}: need 8-bit RGB/RGBA, non-interlaced")
            bpp = 4 if ctype == 6 else 3
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
    raw = zlib.decompress(b"".join(idat))
    stride = w * bpp
    out, prev, i = bytearray(), bytearray(stride), 0
    for _ in range(h):
        f = raw[i]
        line = bytearray(raw[i + 1:i + 1 + stride])
        i += 1 + stride
        for x in range(stride):
            a = line[x - bpp] if x >= bpp else 0
            b = prev[x]
            c = prev[x - bpp] if x >= bpp else 0
            if f == 1:
                line[x] = (line[x] + a) & 255
            elif f == 2:
                line[x] = (line[x] + b) & 255
            elif f == 3:
                line[x] = (line[x] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        out += line
        prev = line
    if bpp == 3:
        out = bytearray(v for j in range(0, len(out), 3) for v in (*out[j:j + 3], 255))
    return w, h, out


def arg(name, default=None):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else default


def main():
    if "--count-new" in sys.argv:
        colour = bytes(int(v) for v in arg("--count-new").split(","))
        a_path, b_path = [a for a in sys.argv[1:] if a.endswith(".png")][:2]
        w, h, a = read_png(a_path)
        wb, hb, b = read_png(b_path)
        if (w, h) != (wb, hb):
            sys.exit(f"size mismatch {w}x{h} vs {wb}x{hb}")
        lo, hi = (int(v) for v in arg("--rows", f"0,{h}").split(","))
        n = 0
        for y in range(lo, hi):
            for x in range(w):
                i = (y * w + x) * 4
                if a[i:i + 4] == colour and b[i:i + 4] != colour:
                    n += 1
        print(f"new={n}")
        return 0
    if "--count" in sys.argv:
        colour = bytes(int(v) for v in arg("--count").split(","))
        path = [a for a in sys.argv[1:] if a.endswith(".png")][0]
        w, h, px = read_png(path)
        lo, hi = (int(v) for v in arg("--rows", f"0,{h}").split(","))
        n = sum(1 for y in range(lo, hi) for x in range(w) if px[(y * w + x) * 4:(y * w + x) * 4 + 4] == colour)
        print(f"matching={n}")
        return 0
    a_path, b_path = [a for a in sys.argv[1:] if a.endswith(".png")][:2]
    tol = int(arg("--tolerance", "0"))
    wa, ha, a = read_png(a_path)
    wb, hb, b = read_png(b_path)
    if (wa, ha) != (wb, hb):
        sys.exit(f"size mismatch {wa}x{ha} vs {wb}x{hb}")
    mask = arg("--mask")
    rects = []
    if mask is not None:
        for r in filter(None, (r.strip() for r in mask.split(";"))):
            x0, y0, x1, y1 = (float(v) for v in r.split(","))
            rects.append((x0, y0, x1, y1))
    differing = worst = outside = 0
    for p in range(0, len(a), 4):
        d = max(abs(a[p + k] - b[p + k]) for k in range(4))
        worst = max(worst, d)
        if d > tol:
            differing += 1
            if mask is not None:
                x, y = (p // 4) % wa, (p // 4) // wa
                if not any(x0 <= x < x1 and y0 <= y < y1 for x0, y0, x1, y1 in rects):
                    outside += 1
    line = f"differing={differing} worst={worst} pixels={wa * ha}"
    if mask is not None:
        line += f" outside={outside}"
    print(line)
    if outside:
        return 1
    lo, hi = arg("--min-differing"), arg("--max-differing")
    if lo is not None and differing < int(lo):
        return 1
    if hi is not None and differing > int(hi):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Build the Lightair brand assets from geometry and the Inter font.

Outputs (in branding/final/):
  icon/      lightair-icon.svg, lightair-icon-small.svg, PNGs, Lightair.iconset, Lightair.icns
  wordmark/  lightair-wordmark-{ink,white}.svg + PNGs
  lockup/    lightair-lockup-{ink,white}.svg + PNGs
  board.png  overview of everything

Run from anywhere:  python3 branding/source/build.py
Needs: fontTools, Pillow, macOS iconutil (for .icns).
"""
import math
import os
import shutil
import subprocess

from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.recordingPen import DecomposingRecordingPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.varLib import instancer
from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, "final")

# ---------------------------------------------------------------- palette
INK = "#141414"
WHITE = "#FFFFFF"
RED = "#FF3B4A"
EDGE = "#2A2A2D"  # hairline that keeps the dark icon visible on dark backgrounds
PAPER = "#F5F5F7"


def rgb(hex_):
    h = hex_.lstrip("#")
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


# ---------------------------------------------------------------- squircle
def squircle_ops(size, radius, smoothing, ox, oy):
    """Continuous-corner rounded square (Figma-style corner smoothing) as path ops."""
    s = smoothing
    p = (1 + s) * radius
    arc_measure = 90 * (1 - s)
    arc = math.sin(math.radians(arc_measure / 2)) * radius * math.sqrt(2)
    alpha = (90 - arc_measure) / 2
    p34 = radius * math.tan(math.radians(alpha / 2))
    beta = 45 * s
    c = p34 * math.cos(math.radians(beta))
    d = c * math.tan(math.radians(beta))
    b = (p - arc - c - d) / 3
    a = 2 * b

    w = size
    # top-right corner in local coords (y down), starting on the top edge
    start = (w - p, 0.0)
    p1 = (start[0] + a + b + c, d)
    c1 = ((start[0] + a, 0.0), (start[0] + a + b, 0.0), p1)
    p2 = (p1[0] + arc, p1[1] + arc)
    p3 = (p2[0] + d, p2[1] + a + b + c)
    c2 = ((p2[0] + d, p2[1] + c), (p2[0] + d, p2[1] + b + c), p3)
    centre = (w - radius, radius)

    cx = cy = w / 2

    def rot(pt, k):  # rotate k * 90 degrees clockwise around the square's centre
        x, y = pt[0] - cx, pt[1] - cy
        for _ in range(k):
            x, y = -y, x
        return (x + cx + ox, y + cy + oy)

    ops = []
    for k in range(4):
        st = rot(start, k)
        ops.append(("M", st) if k == 0 else ("L", st))
        ops.append(("C", rot(c1[0], k), rot(c1[1], k), rot(c1[2], k)))
        ops.append(("A", radius, rot(p2, k), rot(centre, k)))
        ops.append(("C", rot(c2[0], k), rot(c2[1], k), rot(c2[2], k)))
    ops.append(("Z",))
    return ops


def ops_to_svg(ops):
    f = lambda v: f"{v:.2f}".rstrip("0").rstrip(".")
    out = []
    for op in ops:
        if op[0] == "M":
            out.append(f"M{f(op[1][0])} {f(op[1][1])}")
        elif op[0] == "L":
            out.append(f"L{f(op[1][0])} {f(op[1][1])}")
        elif op[0] == "C":
            out.append("C" + " ".join(f"{f(x)} {f(y)}" for x, y in op[1:]))
        elif op[0] == "A":
            r, end = op[1], op[2]
            out.append(f"A{f(r)} {f(r)} 0 0 1 {f(end[0])} {f(end[1])}")
        elif op[0] == "Z":
            out.append("Z")
    return "".join(out)


def ops_to_polygon(ops, scale=1.0):
    pts = []
    cur = None
    for op in ops:
        if op[0] in ("M", "L"):
            cur = op[1]
            pts.append(cur)
        elif op[0] == "C":
            p0, (p1, p2, p3) = cur, op[1:]
            for i in range(1, 33):
                t = i / 32
                mt = 1 - t
                pts.append((
                    mt**3 * p0[0] + 3 * mt * mt * t * p1[0] + 3 * mt * t * t * p2[0] + t**3 * p3[0],
                    mt**3 * p0[1] + 3 * mt * mt * t * p1[1] + 3 * mt * t * t * p2[1] + t**3 * p3[1],
                ))
            cur = p3
        elif op[0] == "A":
            r, end, c = op[1], op[2], op[3]
            a0 = math.atan2(cur[1] - c[1], cur[0] - c[0])
            a1 = math.atan2(end[1] - c[1], end[0] - c[0])
            while a1 < a0:
                a1 += 2 * math.pi
            for i in range(1, 25):
                t = a0 + (a1 - a0) * i / 24
                pts.append((c[0] + r * math.cos(t), c[1] + r * math.sin(t)))
            cur = end
    return [(x * scale, y * scale) for x, y in pts]


# ---------------------------------------------------------------- icon
CANVAS = 1024
BODY = 824            # macOS icon grid: 824 px body centred on a 1024 canvas
BODY_OFF = (CANVAS - BODY) / 2
RADIUS = 185.4
SMOOTH = 0.6
HAIR = 6

# The L-light: a bold geometric L with the on-air light in its corner. Fractions of the body
# size: the L's box (left, top, right, bottom), its stroke, and the dot (radius, and its centre's
# inset from the box's top-right). "small" is tuned for 16 and 32 px renders: a heavier L, a
# bigger light, no hairline.
ICON_VARIANTS = {
    "regular": dict(box=(0.28, 0.24, 0.72, 0.76), stroke=0.155, dot=0.105, inset=(0.085, 0.13), hairline=True),
    "small": dict(box=(0.25, 0.21, 0.75, 0.79), stroke=0.20, dot=0.125, inset=(0.095, 0.13), hairline=False),
}


def icon_parts(variant, size=CANVAS, body=BODY, off=BODY_OFF, radius=RADIUS):
    v = ICON_VARIANTS[variant]
    k = size / CANVAS
    body, off, radius = body * k, off * k, radius * k
    parts = []
    if v["hairline"]:
        hair = HAIR * k
        parts.append(("path", squircle_ops(body, radius, SMOOTH, off, off), EDGE))
        inner = body - 2 * hair
        parts.append(("path", squircle_ops(inner, radius * inner / body, SMOOTH, off + hair, off + hair), INK))
    else:
        parts.append(("path", squircle_ops(body, radius, SMOOTH, off, off), INK))
    x0, y0, x1, y1 = (off + f * body for f in v["box"])
    t = v["stroke"] * body
    parts.append(("rrect", (x0, y0, x0 + t, y1, t / 2), WHITE))   # the stem
    parts.append(("rrect", (x0, y1 - t, x1, y1, t / 2), WHITE))   # the foot
    ix, iy = v["inset"]
    parts.append(("circle", (x1 - ix * body, y0 + iy * body, v["dot"] * body), RED))
    return parts


def parts_to_svg(parts, w, h, extra=""):
    body = []
    for kind, geo, col in parts:
        if kind == "path":
            body.append(f'  <path d="{ops_to_svg(geo)}" fill="{col}"/>')
        elif kind == "circle":
            cx, cy, r = geo
            body.append(f'  <circle cx="{cx:.2f}" cy="{cy:.2f}" r="{r:.2f}" fill="{col}"/>')
        elif kind == "ring":
            cx, cy, r, sw = geo
            body.append(f'  <circle cx="{cx:.2f}" cy="{cy:.2f}" r="{r:.2f}" fill="none" stroke="{col}" stroke-width="{sw:.2f}"/>')
        elif kind == "rrect":
            x0, y0, x1, y1, r = geo
            body.append(f'  <rect x="{x0:.2f}" y="{y0:.2f}" width="{x1 - x0:.2f}" height="{y1 - y0:.2f}" rx="{r:.2f}" fill="{col}"/>')
        elif kind == "raw":
            body.append(geo)
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0f}" height="{h:.0f}" '
            f'viewBox="0 0 {w:.2f} {h:.2f}">\n' + "\n".join(body) + extra + "\n</svg>\n")


def draw_parts(img, parts, scale):
    d = ImageDraw.Draw(img)
    for kind, geo, col in parts:
        if kind == "path":
            d.polygon(ops_to_polygon(geo, scale), fill=rgb(col))
        elif kind == "circle":
            cx, cy, r = (v * scale for v in geo)
            d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=rgb(col))
        elif kind == "rrect":
            x0, y0, x1, y1, r = (v * scale for v in geo)
            d.rounded_rectangle((x0, y0, x1, y1), radius=r, fill=rgb(col))
        elif kind == "ring":
            cx, cy, r, sw = (v * scale for v in geo)
            ro, ri = r + sw / 2, r - sw / 2
            # ring = outer disc minus inner disc, drawn on a mask so the body shows through
            mask = Image.new("L", img.size, 0)
            md = ImageDraw.Draw(mask)
            md.ellipse((cx - ro, cy - ro, cx + ro, cy + ro), fill=255)
            md.ellipse((cx - ri, cy - ri, cx + ri, cy + ri), fill=0)
            img.paste(Image.new("RGBA", img.size, rgb(col) + (255,)), (0, 0), mask)


def render_parts(parts, w, h, px_w, ss=4):
    scale = px_w * ss / w
    img = Image.new("RGBA", (round(w * scale), round(h * scale)), (0, 0, 0, 0))
    draw_parts(img, parts, scale)
    return img.resize((px_w, round(h * px_w / w)), Image.LANCZOS)


def build_icon():
    d = os.path.join(OUT, "icon")
    os.makedirs(d, exist_ok=True)
    for variant, name in (("regular", "lightair-icon"), ("small", "lightair-icon-small")):
        with open(os.path.join(d, name + ".svg"), "w") as fh:
            fh.write(parts_to_svg(icon_parts(variant), CANVAS, CANVAS))
    for px in (1024, 512, 256, 128):
        render_parts(icon_parts("regular"), CANVAS, CANVAS, px).save(os.path.join(d, f"lightair-icon-{px}.png"))
    for px in (64, 32, 16):
        variant = "regular" if px >= 64 else "small"
        render_parts(icon_parts(variant), CANVAS, CANVAS, px, ss=8).save(os.path.join(d, f"lightair-icon-{px}.png"))

    iconset = os.path.join(d, "Lightair.iconset")
    shutil.rmtree(iconset, ignore_errors=True)
    os.makedirs(iconset)
    for base in (16, 32, 128, 256, 512):
        for mult in (1, 2):
            px = base * mult
            variant = "small" if px <= 32 else "regular"
            name = f"icon_{base}x{base}{'@2x' if mult == 2 else ''}.png"
            render_parts(icon_parts(variant), CANVAS, CANVAS, px, ss=8 if px <= 64 else 4).save(os.path.join(iconset, name))
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(d, "Lightair.icns")], check=True)


# ---------------------------------------------------------------- wordmark
TEXT = "Lightair"
WEIGHT = 600          # Inter SemiBold
TRACKING = -0.02      # em; Inter reads best slightly tight at display sizes
DOT_SCALE = 1.18      # the red on-air dot is a little larger than Inter's tittle
EM = 1000.0           # wordmark geometry is laid out at a 1000-unit em


def static_inter():
    path = os.path.join(HERE, "fonts", f"Inter-{WEIGHT}.ttf")
    if not os.path.exists(path):
        vf = TTFont(os.path.join(HERE, "fonts", "Inter-Variable.ttf"))
        instancer.instantiateVariableFont(vf, {"wght": WEIGHT, "slnt": 0}, inplace=True)
        vf.save(path)
    return path


def pair_kern(font, left, right):
    gpos = font["GPOS"].table
    idx = set()
    for fr in gpos.FeatureList.FeatureRecord:
        if fr.FeatureTag == "kern":
            idx.update(fr.Feature.LookupListIndex)
    for li in sorted(idx):
        lk = gpos.LookupList.Lookup[li]
        for st in lk.SubTable:
            if lk.LookupType == 9:
                if st.ExtensionLookupType != 2:
                    continue
                st = st.ExtSubTable
            elif lk.LookupType != 2:
                continue
            cov = st.Coverage.glyphs
            if left not in cov:
                continue
            if st.Format == 1:
                for pvr in st.PairSet[cov.index(left)].PairValueRecord:
                    if pvr.SecondGlyph == right:
                        return getattr(pvr.Value1, "XAdvance", 0) or 0
            elif st.Format == 2:
                c1 = st.ClassDef1.classDefs.get(left, 0)
                c2 = st.ClassDef2.classDefs.get(right, 0)
                rec = st.Class1Record[c1].Class2Record[c2]
                v = getattr(rec.Value1, "XAdvance", 0) or 0
                if v:
                    return v
    return 0


def layout_wordmark():
    """Return glyph placements in em units (x pen positions), plus font metrics."""
    path = static_inter()
    font = TTFont(path)
    upm = font["head"].unitsPerEm
    k = EM / upm
    cmap = font.getBestCmap()
    names = [cmap[ord(ch)] for ch in TEXT]
    hmtx = font["hmtx"]
    x = 0.0
    placed = []
    for i, g in enumerate(names):
        placed.append((g, x))
        adv = hmtx[g][0]
        if i + 1 < len(names):
            adv += pair_kern(font, g, names[i + 1])
            adv += TRACKING * upm
        x += adv * k
    os2 = font["OS/2"]
    metrics = dict(upm=upm, k=k, cap=os2.sCapHeight * k, xh=os2.sxHeight * k)
    return font, path, placed, metrics


def contours(glyphset, name):
    rec = DecomposingRecordingPen(glyphset)
    glyphset[name].draw(rec)
    out, cur = [], []
    for op, args in rec.value:
        cur.append((op, args))
        if op in ("closePath", "endPath"):
            out.append(cur)
            cur = []
    return out


def replay(ops, pen):
    for op, args in ops:
        getattr(pen, op)(*args)


def wordmark_geometry():
    """Ink paths (SVG, y-down, baseline at y=0) and the red dot circle, in em units."""
    font, path, placed, m = layout_wordmark()
    gs = font.getGlyphSet()
    k = m["k"]
    first_i = TEXT.index("i")
    ink_paths = []
    dot = None
    for idx, (g, x) in enumerate(placed):
        svg = SVGPathPen(gs)
        tpen = TransformPen(svg, (k, 0, 0, -k, x, 0))
        for cont in contours(gs, g):
            bp = BoundsPen(gs)
            replay(cont, bp)
            xmin, ymin, xmax, ymax = bp.bounds
            if idx == first_i and ymin > font["OS/2"].sxHeight:
                # the tittle of the first i becomes the red on-air dot
                r0 = (xmax - xmin) / 2 * k
                r = r0 * DOT_SCALE
                cx = x + (xmin + xmax) / 2 * k
                cy = -((ymin + ymax) / 2 * k) - (r - r0)
                dot = (cx, cy, r)
                continue
            replay(cont, tpen)
        ink_paths.append(svg.getCommands())
    # bounds of the ink, in em units (y-down)
    xs, ys = [], []
    for g, x in placed:
        bp = BoundsPen(gs)
        gs[g].draw(bp)
        if bp.bounds:
            xmin, ymin, xmax, ymax = bp.bounds
            xs += [x + xmin * k, x + xmax * k]
            ys += [-ymax * k, -ymin * k]
    xs += [dot[0] - dot[2], dot[0] + dot[2]]
    ys += [dot[1] - dot[2]]
    bbox = (min(xs), min(ys), max(xs), max(ys))
    return dict(paths=ink_paths, dot=dot, bbox=bbox, placed=placed, font_path=path, metrics=m, font=font)


def wordmark_svg_group(geo, colour, dx, dy, scale=1.0):
    d = " ".join(p for p in geo["paths"] if p)
    cx, cy, r = geo["dot"]
    t = f'transform="translate({dx:.2f} {dy:.2f}) scale({scale:.5f})"'
    return (f'  <g {t}>\n    <path d="{d}" fill="{colour}"/>\n'
            f'    <circle cx="{cx:.2f}" cy="{cy:.2f}" r="{r:.2f}" fill="{RED}"/>\n  </g>')


def render_wordmark(geo, colour, px_h, ss=4):
    """Raster the wordmark with FreeType (correct non-zero fill) at px_h ink height."""
    x0, y0, x1, y1 = geo["bbox"]
    s = px_h * ss / (y1 - y0)                      # px per em unit
    W, H = round((x1 - x0) * s), round((y1 - y0) * s)
    img = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    f = ImageFont.truetype(geo["font_path"], size=round(EM * s))
    first_i = TEXT.index("i")
    for idx, (g, x) in enumerate(geo["placed"]):
        ch = "ı" if idx == first_i else TEXT[idx]   # dotless i, the red dot is drawn separately
        d.text(((x - x0) * s, (0 - y0) * s), ch, font=f, fill=rgb(colour), anchor="ls")
    cx, cy, r = geo["dot"]
    cx, cy, r = (cx - x0) * s, (cy - y0) * s, r * s
    d.ellipse((cx - r, cy - r, cx + r, cy + r), fill=rgb(RED))
    return img.resize((round(W / ss), round(H / ss)), Image.LANCZOS)


def build_wordmark(geo):
    d = os.path.join(OUT, "wordmark")
    os.makedirs(d, exist_ok=True)
    x0, y0, x1, y1 = geo["bbox"]
    w, h = x1 - x0, y1 - y0
    for name, col in (("ink", INK), ("white", WHITE)):
        svg = parts_to_svg([("raw", wordmark_svg_group(geo, col, -x0, -y0), None)], w, h)
        with open(os.path.join(d, f"lightair-wordmark-{name}.svg"), "w") as fh:
            fh.write(svg)
        for px in (120, 240, 480):
            render_wordmark(geo, col, px).save(os.path.join(d, f"lightair-wordmark-{name}-{px}h.png"))


# ---------------------------------------------------------------- lockup
def lockup_layout(geo):
    m = geo["metrics"]
    x0, y0, x1, y1 = geo["bbox"]
    icon = m["cap"] * 1.6                 # icon body height relative to cap height
    gap = icon * 0.38
    centre_y = -m["cap"] / 2              # icon centred on the cap height
    top = min(y0, centre_y - icon / 2)
    bottom = max(y1, centre_y + icon / 2)
    return dict(icon=icon, gap=gap, cy=centre_y, top=top, bottom=bottom,
                w=icon + gap + (x1 - x0), h=bottom - top)


def build_lockup(geo):
    d = os.path.join(OUT, "lockup")
    os.makedirs(d, exist_ok=True)
    L = lockup_layout(geo)
    x0, y0, x1, y1 = geo["bbox"]
    s = L["icon"] / BODY
    icon_y = L["cy"] - L["icon"] / 2 - L["top"]
    for name, col in (("ink", INK), ("white", WHITE)):
        parts = icon_parts("regular", size=CANVAS * s, off=0)  # icon_parts scales body/radius by size/CANVAS
        # shift icon parts down to icon_y
        shifted = []
        for kind, g, c in parts:
            if kind == "path":
                g = [(op[0],) + tuple(((p[0], p[1] + icon_y) if isinstance(p, tuple) else p) for p in op[1:]) for op in g]
            elif kind in ("circle", "ring"):
                g = (g[0], g[1] + icon_y) + tuple(g[2:])
            elif kind == "rrect":
                g = (g[0], g[1] + icon_y, g[2], g[3] + icon_y, g[4])
            shifted.append((kind, g, c))
        word = ("raw", wordmark_svg_group(geo, col, L["icon"] + L["gap"] - x0, -L["top"]), None)
        svg = parts_to_svg(shifted + [word], L["w"], L["h"])
        with open(os.path.join(d, f"lightair-lockup-{name}.svg"), "w") as fh:
            fh.write(svg)
        for px_h in (120, 240):
            k = px_h / L["h"]
            img = Image.new("RGBA", (round(L["w"] * k), px_h), (0, 0, 0, 0))
            ic = render_parts(shifted, BODY * s, L["h"], round(BODY * s * k))
            img.alpha_composite(ic.crop((0, 0, min(ic.width, img.width), min(ic.height, img.height))), (0, 0))
            wm = render_wordmark(geo, col, round((y1 - y0) * k))
            img.alpha_composite(wm, (round((L["icon"] + L["gap"]) * k), round((y0 - L["top"]) * k)))
            img.save(os.path.join(d, f"lightair-lockup-{name}-{px_h}h.png"))


# ---------------------------------------------------------------- board
def build_board(geo):
    def font(sz):
        for p in ("/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/Helvetica.ttc"):
            try:
                return ImageFont.truetype(p, sz)
            except OSError:
                pass
        return ImageFont.load_default()

    W, H = 1800, 1700
    board = Image.new("RGB", (W, H), rgb(PAPER))
    dk = Image.new("RGB", (W // 2, H), rgb(INK))
    board.paste(dk, (W // 2, 0))
    d = ImageDraw.Draw(board)
    d.text((60, 40), "Lightair · brand", font=font(40), fill=rgb(INK))
    d.text((W // 2 + 60, 40), "on dark", font=font(26), fill=(160, 160, 165))
    for half, bg in ((0, PAPER), (1, INK)):
        ox = half * W // 2
        ic = Image.open(os.path.join(OUT, "icon", "lightair-icon-512.png"))
        ic = ic.resize((400, 400), Image.LANCZOS)
        board.paste(ic, (ox + 60, 120), ic)
        x = ox + 500
        for px in (128, 64, 32, 16):
            sm = Image.open(os.path.join(OUT, "icon", f"lightair-icon-{px}.png"))
            board.paste(sm, (x, 320 - px // 2), sm)
            x += px + 30
        col = "ink" if half == 0 else "white"
        wm = Image.open(os.path.join(OUT, "wordmark", f"lightair-wordmark-{col}-240h.png"))
        wm.thumbnail((W // 2 - 120, 200))
        board.paste(wm, (ox + (W // 2 - wm.width) // 2, 640), wm)
        lk = Image.open(os.path.join(OUT, "lockup", f"lightair-lockup-{col}-240h.png"))
        lk.thumbnail((W // 2 - 120, 170))
        board.paste(lk, (ox + (W // 2 - lk.width) // 2, 960), lk)
        sm = Image.open(os.path.join(OUT, "lockup", f"lightair-lockup-{col}-120h.png"))
        sm.thumbnail((W // 2 - 120, 60))
        board.paste(sm, (ox + 60, 1250), sm)
        wm2 = Image.open(os.path.join(OUT, "wordmark", f"lightair-wordmark-{col}-120h.png"))
        wm2.thumbnail((400, 32))
        board.paste(wm2, (ox + 60, 1370), wm2)
    sw = [(INK, "Ink #141414"), (WHITE, "White #FFFFFF"), (RED, "On-air red #FF3B4A"), (EDGE, "Edge #2A2A2D")]
    for i, (c, label) in enumerate(sw):
        x = 60 + i * 220
        d.rectangle((x, 1480, x + 60, 1540), fill=rgb(c), outline=(200, 200, 205))
        d.text((x, 1555), label, font=font(20), fill=rgb(INK))
    board.save(os.path.join(OUT, "board.png"))


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    build_icon()
    geo = wordmark_geometry()
    build_wordmark(geo)
    build_lockup(geo)
    build_board(geo)
    print("built", OUT)

#!/usr/bin/env python3
"""Render a tmux `capture-pane -e` dump as a terminal-window PNG via headless Chrome."""

import html
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata

from PIL import Image

ansi_path, png_path, title = sys.argv[1], sys.argv[2], sys.argv[3]
cursor = open(ansi_path.replace(".ansi", ".cursor")).read().strip().split(",")
work = tempfile.mkdtemp(prefix="sessift-render-")

BG, FG = "#262427", "#fcfcfa"  # Ghostty theme 0x96f
BASE = ["#262427", "#ff666d", "#b3e03a", "#ffc739", "#00cde8", "#a392e8", "#9deaf6", "#fcfcfa",
        "#545452", "#ff7e83", "#bee55e", "#ffd05e", "#1bd5eb", "#b0a3eb", "#acedf8", "#fcfcfa"]


def color(i):
    if i < 16:
        return BASE[i]
    if i < 232:
        i -= 16
        lv = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (lv[i // 36], lv[i // 6 % 6], lv[i % 6])
    g = 8 + 10 * (i - 232)
    return "#%02x%02x%02x" % (g, g, g)


def width(c):
    return 2 if unicodedata.east_asian_width(c) in ("W", "F") else 1


BOX = {"─": "h", "│": "v", "╭": "tl", "╮": "tr", "╰": "bl", "╯": "br", "▌": "half"}

rows = []
for line in open(ansi_path).read().split("\n"):
    cells, st = [], {}
    for tok in re.split(r"(\x1b\[[0-9;]*m)", line):
        if tok.startswith("\x1b["):
            ps = [int(p) if p else 0 for p in tok[2:-1].split(";")]
            i = 0
            while i < len(ps):
                p = ps[i]
                if p == 0:
                    st = {}
                elif p == 1:
                    st["b"] = 1
                elif p == 2:
                    st["dim"] = 1
                elif p == 3:
                    st["i"] = 1
                elif p == 4:
                    st["u"] = 1
                elif p == 7:
                    st["rev"] = 1
                elif p == 22:
                    st.pop("b", None); st.pop("dim", None)
                elif p == 23:
                    st.pop("i", None)
                elif p == 24:
                    st.pop("u", None)
                elif p == 27:
                    st.pop("rev", None)
                elif 30 <= p <= 37:
                    st["fg"] = color(p - 30)
                elif 90 <= p <= 97:
                    st["fg"] = color(p - 90 + 8)
                elif 40 <= p <= 47:
                    st["bg"] = color(p - 40)
                elif 100 <= p <= 107:
                    st["bg"] = color(p - 100 + 8)
                elif p == 39:
                    st.pop("fg", None)
                elif p == 49:
                    st.pop("bg", None)
                elif p in (38, 48) and i + 1 < len(ps):
                    if ps[i + 1] == 5:
                        st["fg" if p == 38 else "bg"] = color(ps[i + 2]); i += 2
                    elif ps[i + 1] == 2:
                        st["fg" if p == 38 else "bg"] = "#%02x%02x%02x" % tuple(ps[i + 2:i + 5]); i += 4
                i += 1
        else:
            for c in tok:
                cells.append((c, dict(st)))
    rows.append(cells)
while rows and not rows[-1]:
    rows.pop()

CELL_W, CELL_H, FONT = 9.0, 19, 15  # Maple Mono advance is 0.6em
cols = max(sum(width(c) for c, _ in r) for r in rows)
out = []
for y, r in enumerate(rows):
    x = 0
    for c, st in r:
        w = width(c)
        fg, bg = st.get("fg", FG), st.get("bg")
        if st.get("rev"):
            fg, bg = bg or BG, fg
        style = [f"left:{x * CELL_W}px", f"top:{y * CELL_H}px", f"width:{w * CELL_W}px", f"color:{fg}"]
        if bg:
            style.append(f"background:{bg}")
        if st.get("b"):
            style.append("font-weight:700")
        if st.get("u"):
            style.append("text-decoration:underline")
        cls = BOX.get(c)
        if cls:
            out.append(f'<i class="c {cls}" style="{";".join(style)}"></i>')
        elif c != " " or bg:
            out.append(f'<i class="c" style="{";".join(style)}">{html.escape(c) if c != " " else ""}</i>')
        x += w
if cursor[2] == "1":
    cx, cy = int(cursor[0]), int(cursor[1])
    out.append(f'<i class="cursor" style="left:{cx * CELL_W}px;top:{cy * CELL_H}px"></i>')

W, H = cols * CELL_W, len(rows) * CELL_H
PAD = 14
doc = f"""<!doctype html><meta charset="utf-8"><style>
html,body{{margin:0;background:transparent}}
.win{{position:absolute;left:40px;top:30px;width:{W + 2 * PAD}px;background:{BG};border-radius:10px;
  border:1px solid #3a383b;overflow:hidden}}
.bar{{height:30px;position:relative;background:#2e2c2f}}
.dot{{position:absolute;top:10px;width:11px;height:11px;border-radius:50%}}
.ttl{{position:absolute;left:0;right:0;top:0;line-height:30px;text-align:center;color:#a8a6a8;
  font:600 12px -apple-system,"PingFang SC",sans-serif}}
.term{{position:relative;margin:{PAD}px;width:{W}px;height:{H}px;
  font-family:"Maple Mono NF CN","PingFang SC",monospace;font-size:{FONT}px;line-height:{CELL_H}px}}
.c{{position:absolute;height:{CELL_H}px;font-style:normal;text-align:center;white-space:pre;overflow:visible}}
.c.h::before,.c.v::before,.c.tl::before,.c.tr::before,.c.bl::before,.c.br::before,.c.half::before{{
  content:"";position:absolute;border:0 solid currentColor}}
.c.h::before{{left:0;right:0;top:50%;border-top-width:1px}}
.c.v::before{{top:0;bottom:0;left:50%;border-left-width:1px}}
.c.tl::before{{left:50%;right:0;top:50%;bottom:0;border-left-width:1px;border-top-width:1px;border-top-left-radius:5px}}
.c.tr::before{{left:0;right:50%;top:50%;bottom:0;border-right-width:1px;border-top-width:1px;border-top-right-radius:5px;margin-right:-1px}}
.c.bl::before{{left:50%;right:0;top:0;bottom:50%;border-left-width:1px;border-bottom-width:1px;border-bottom-left-radius:5px}}
.c.br::before{{left:0;right:50%;top:0;bottom:50%;border-right-width:1px;border-bottom-width:1px;border-bottom-right-radius:5px;margin-right:-1px}}
.c.half::before{{left:0;width:45%;top:0;bottom:0;background:currentColor}}
.cursor{{position:absolute;width:2px;height:{CELL_H}px;background:{FG}}}
</style>
<div class="win"><div class="bar">
<span class="dot" style="left:12px;background:#ff5f57"></span>
<span class="dot" style="left:30px;background:#febc2e"></span>
<span class="dot" style="left:48px;background:#28c840"></span>
<div class="ttl">{html.escape(title)}</div></div>
<div class="term">{"".join(out)}</div></div>"""
page = os.path.join(work, "page.html")
open(page, "w").write(doc)
win_w, win_h = int(W + 2 * PAD + 80), int(H + 2 * PAD + 30 + 80)
chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
raw = os.path.join(work, "raw.png")
subprocess.run([chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run",
                f"--user-data-dir={work}/chrome", "--force-device-scale-factor=2",
                "--default-background-color=00000000", f"--window-size={win_w},{win_h}",
                f"--screenshot={raw}", "file://" + page], check=True, capture_output=True)
subprocess.run(["magick", raw, "-trim", "+repage", png_path], check=True)
shutil.rmtree(work, ignore_errors=True)
# Three quarters of the 2x pixels and a 256-colour palette (alpha kept): well under 150KB each.
im = Image.open(png_path).convert("RGBA")
im = im.resize((im.width // 2 * 3 // 2, im.height // 2 * 3 // 2), Image.LANCZOS)
im.quantize(256, method=Image.Quantize.FASTOCTREE, dither=Image.Dither.NONE).save(png_path, optimize=True)
print(png_path)

#!/usr/bin/env python3
"""Turn a scene recorded by `record.py` into a GIF with the keys pressed shown under the window.

  gif.py OUT SCENE DEST.gif [TITLE]

SCALE (default 1.25) sets the pixel density; frames stay up for the time `record.py` gave them.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

from playwright.sync_api import sync_playwright

import term

OUT, SCENE, DEST = sys.argv[1], sys.argv[2], sys.argv[3]
TITLE = sys.argv[4] if len(sys.argv) > 4 else "sessift"
SCALE = float(os.environ.get("SCALE", 1.25))
STAGE = "#19181b"

spec = json.load(open(os.path.join(OUT, SCENE + ".json")))
size = tuple(spec["size"])
w, h = term.page_size(size)
work = tempfile.mkdtemp(prefix="sessift-gif-")
listing = []
with sync_playwright() as p:
    browser = p.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": w, "height": h}, device_scale_factor=SCALE)
    for i, frame in enumerate(spec["frames"]):
        base = os.path.join(OUT, frame["ansi"])
        rows = term.parse(open(base).read())
        cursor = open(base.replace(".ansi", ".cursor")).read().strip().split(",")
        doc = term.document(rows, cursor, TITLE, size=size, keys=frame["keys"], caption=frame["caption"])
        doc = doc.replace("</style>", f".stage{{background:{STAGE};height:{h}px}}</style>")
        page.set_content(doc)
        page.evaluate("document.fonts.ready")
        png = os.path.join(work, f"{i:03d}.png")
        page.screenshot(path=png, clip={"x": 0, "y": 0, "width": w, "height": h})
        listing.append(f"file '{png}'\nduration {frame['hold']}\n")
    browser.close()
# The concat demuxer ignores the last duration unless the last file is listed again.
listing.append(f"file '{os.path.join(work, f'{len(spec['frames']) - 1:03d}.png')}'\n")
open(os.path.join(work, "list.txt"), "w").write("".join(listing))
subprocess.run(
    ["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", os.path.join(work, "list.txt"),
     "-vf", "split[a][b];[a]palettegen=max_colors=256:stats_mode=full[p];"
            "[b][p]paletteuse=dither=none:diff_mode=rectangle",
     "-fps_mode", "vfr", "-loop", "0", DEST],
    check=True)
shutil.rmtree(work, ignore_errors=True)
total = sum(f["hold"] for f in spec["frames"])
print(f"{DEST}: {len(spec['frames'])} frames, {total:.1f}s, {os.path.getsize(DEST) / 1e6:.2f}MB")

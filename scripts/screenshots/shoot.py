#!/usr/bin/env python3
"""Drive the real TUI inside an isolated tmux server and capture screens as ANSI."""

import os
import subprocess
import sys
import time
import unicodedata

DEMO, BIN, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
COLS, ROWS = int(os.environ.get("COLS", 124)), int(os.environ.get("ROWS", 32))
SOCK = "sft-shots"
os.makedirs(OUT, exist_ok=True)
conf = os.path.join(OUT, "tmux.conf")
open(conf, "w").write("set -g status off\nset -g default-terminal tmux-256color\nset -g escape-time 0\n")


def tmux(*args, check=True):
    return subprocess.run(["tmux", "-L", SOCK, "-f", conf, *args], capture_output=True, text=True, check=check).stdout


def keys(*k):
    tmux("send-keys", "-t", "demo", *k)


def literal(s):
    tmux("send-keys", "-t", "demo", "-l", s)


def width(c):
    return 2 if unicodedata.east_asian_width(c) in ("W", "F") else 1


def locate(text):
    for y, line in enumerate(tmux("capture-pane", "-p", "-t", "demo").split("\n")):
        i = line.find(text)
        if i >= 0:
            return sum(width(c) for c in line[:i]), y
    raise SystemExit(f"{text!r} not on screen")


def mouse(code, x, y, release=True):
    literal(f"\x1b[<{code};{x + 1};{y + 1}M")
    if release:
        literal(f"\x1b[<{code};{x + 1};{y + 1}m")


def capture(name):
    ansi = tmux("capture-pane", "-p", "-e", "-N", "-t", "demo")
    cursor = tmux("display", "-p", "-t", "demo", "#{cursor_x},#{cursor_y},#{cursor_flag}").strip()
    open(os.path.join(OUT, name + ".ansi"), "w").write(ansi)
    open(os.path.join(OUT, name + ".cursor"), "w").write(cursor)
    print("captured", name, "cursor", cursor)


tmux("kill-server", check=False)
env = {"HOME": f"{DEMO}/local/home", "SESSIFT_HOME": f"{DEMO}/local/sessift", "TERM": "tmux-256color", "LANG": "en_US.UTF-8"}
args = ["new-session", "-d", "-s", "demo", "-x", str(COLS), "-y", str(ROWS)]
for k, v in env.items():
    args += ["-e", f"{k}={v}"]
tmux(*args, f"{BIN} pick --no-broker")
time.sleep(2.5)

shots = sys.argv[4:] or ["search", "menu", "reader"]

# 1. Search with the split preview.
literal("回滚")
time.sleep(1.2)
keys("Tab")
time.sleep(1.5)
if "search" in shots:
    capture("search")

# 2. Recent sessions, the source dropdown opened by a click.
keys("Escape")
time.sleep(0.2)
keys("Escape")
time.sleep(1.5)
keys("Down")
keys("Tab")  # split preview off: the full-width list shows source names
keys("Down")
time.sleep(0.8)
x, y = locate("全部来源")
mouse(0, x + 1, y)
time.sleep(0.3)
mx, my = x + 1, y + 3
mouse(35, mx, my, release=False)  # hover the second option
time.sleep(0.4)
if "menu" in shots:
    capture("menu")

# 3. Reading the first session.
keys("Escape")
time.sleep(0.2)
keys("Up")
time.sleep(0.2)
keys("o")
time.sleep(1.5)
if "reader" in shots:
    capture("reader")

keys("q")
time.sleep(0.2)
tmux("kill-server", check=False)

#!/usr/bin/env python3
"""Drive the real TUI in an isolated tmux server, one capture per key press.

  record.py DEMO BIN OUT [scene ...]

Each scene writes OUT/<scene>/NN.ansi and NN.cursor, plus OUT/<scene>.json listing the frames
with the keys pressed, a caption and how long the frame stays up. `gif.py` turns that into a GIF.
"""

import json
import os
import shutil
import subprocess
import sys
import time

# Real paths: herdr starts shells in resolved directories (/tmp is /private/tmp on macOS), and
# the prompt only shortens them to ~ when HOME matches.
DEMO, BIN, OUT = (os.path.realpath(a) for a in sys.argv[1:4])
COLS, ROWS = int(os.environ.get("COLS", 108)), int(os.environ.get("ROWS", 28))
SOCK = "sft-gif"
TMUX = shutil.which("tmux") or "tmux"
os.makedirs(OUT, exist_ok=True)
conf = os.path.join(OUT, "tmux.conf")
open(conf, "w").write("set -g status off\nset -g default-terminal tmux-256color\nset -g escape-time 0\n")
HOME = f"{DEMO}/local/home"

# A plain zsh with the Alt-M binding: the prompt shows a fictional project directory. Resume
# commands name /Users/alice, which the demo maps into its own home, and the agents are stand-ins
# that only say what would happen.
bindir = os.path.join(OUT, "bin")
os.makedirs(bindir, exist_ok=True)
links = {"sessift": BIN, "herdr": shutil.which("herdr") or "/opt/homebrew/bin/herdr"}
for name, target in links.items():
    if target and not os.path.lexists(os.path.join(bindir, name)):
        os.symlink(target, os.path.join(bindir, name))
for agent in ("claude", "codex"):
    stub = os.path.join(bindir, agent)
    open(stub, "w").write(
        "#!/bin/sh\nid=$(printf '%s\\n' \"$@\" | grep -E '^[0-9a-f]{8}-' | head -1)\n"
        f"printf '\\033[2m（演示环境：这里会启动 {agent}，恢复会话 %s…）\\033[0m\\n' \"${{id%%-*}}\"\n")
    os.chmod(stub, 0o755)
zdot = os.path.join(OUT, "zdot")
os.makedirs(zdot, exist_ok=True)
open(os.path.join(zdot, ".zshrc"), "w").write(
    f"export PATH={bindir}:/usr/bin:/bin\n"
    "PROMPT='%F{4}%~%f %F{5}❯%f '\nunsetopt BEEP\n"
    "cd() { local d=\"${1/#\\/Users\\/alice/$HOME}\"; mkdir -p \"$d\"; builtin cd \"$d\"; }\n"
    "eval \"$(sessift shell zsh)\"\n")
project = os.path.join(HOME, "work", "shop-api")
os.makedirs(project, exist_ok=True)

ENV = {
    "HOME": HOME,
    "SESSIFT_HOME": f"{DEMO}/local/sessift",
    "ZDOTDIR": zdot,
    "SHELL": "/bin/zsh",
    "PATH": f"{bindir}:/usr/bin:/bin",
    "TERM": "tmux-256color",
    "LANG": "en_US.UTF-8",
}


def tmux(*args, check=True):
    # A clean environment: nothing from the caller (no HERDR_* pointing at a real herdr).
    return subprocess.run([TMUX, "-L", SOCK, "-f", conf, *args], capture_output=True, text=True,
                          check=check, env=ENV).stdout


def start(command, cwd, cols=None, rows=None):
    global COLS, ROWS
    COLS, ROWS = cols or COLS, rows or ROWS
    tmux("kill-server", check=False)
    args = ["new-session", "-d", "-s", "demo", "-x", str(COLS), "-y", str(ROWS), "-c", cwd]
    for k, v in ENV.items():
        args += ["-e", f"{k}={v}"]
    tmux(*args, command)


class Scene:
    def __init__(self, name):
        self.name, self.frames = name, []
        self.dir = os.path.join(OUT, name)
        os.makedirs(self.dir, exist_ok=True)

    def shot(self, keys, caption, hold):
        n = len(self.frames)
        ansi = tmux("capture-pane", "-p", "-e", "-N", "-t", "demo")
        cursor = tmux("display", "-p", "-t", "demo", "#{cursor_x},#{cursor_y},#{cursor_flag}").strip()
        open(os.path.join(self.dir, f"{n:02d}.ansi"), "w").write(ansi)
        open(os.path.join(self.dir, f"{n:02d}.cursor"), "w").write(cursor)
        self.frames.append({"ansi": f"{self.name}/{n:02d}.ansi", "keys": keys, "caption": caption,
                            "hold": hold})

    def press(self, send, keys, caption, wait=0.6, hold=1.2, literal=False):
        """Send `send` (tmux key names, or text with literal=True), wait, capture."""
        if literal:
            tmux("send-keys", "-t", "demo", "-l", send)
        else:
            tmux("send-keys", "-t", "demo", *send.split())
        time.sleep(wait)
        self.shot(keys, caption, hold)

    def type(self, text, caption, wait=0.45, hold=0.35, last_hold=1.4):
        """One frame per character, the keycap showing what has been typed so far."""
        for i, ch in enumerate(text):
            self.press(ch, [text[:i + 1]], caption, wait=wait,
                       hold=last_hold if i == len(text) - 1 else hold, literal=True)

    def save(self):
        json.dump({"size": [COLS, ROWS], "frames": self.frames},
                  open(os.path.join(OUT, self.name + ".json"), "w"), ensure_ascii=False, indent=1)
        print(self.name, len(self.frames), "frames")


def search():
    """From a shell prompt: Alt-M, search as you type, preview, put the command on the prompt."""
    start("zsh -i", project)
    time.sleep(1.0)
    s = Scene("search")
    s.shot([], "", 1.0)
    s.press("M-m", ["Alt-M"], "在 shell 里打开", wait=2.5, hold=1.6)
    s.type("回滚", "边打字边搜")
    s.press("Tab", ["Tab"], "分栏预览，进入列表", wait=1.2, hold=2.2)
    s.press("Down", ["↓"], "下一个，预览跟着变", wait=0.9, hold=1.6)
    s.press("Up", ["↑"], "回到上一个", wait=0.9, hold=1.0)
    s.press("e", ["e"], "恢复命令放到命令行（Enter 直接执行）", wait=1.2, hold=3.2)
    s.save()


def reader():
    """Search, open the hit in full, expand tool output, find in the session."""
    start(f"{BIN} pick --no-broker", project)
    time.sleep(2.5)
    s = Scene("reader")
    s.shot([], "", 0.8)
    s.type("超时", "搜索")
    s.press("Enter", ["Enter"], "进入列表", wait=0.6, hold=0.9)
    s.press("o", ["o"], "全文阅读，跳到命中的那条", wait=1.5, hold=2.4)
    s.press("t", ["t"], "展开工具输出", wait=0.6, hold=1.8)
    for _ in range(3):
        s.press("j", ["j"], "滚动", wait=0.25, hold=0.45)
    s.press("/", ["/"], "查找", wait=0.4, hold=0.7)
    s.type("缓存", "查找", hold=0.35, last_hold=0.6)
    s.press("Enter", ["Enter"], "跳到匹配处", wait=0.6, hold=2.2)
    s.press("q", ["q"], "回到列表", wait=0.6, hold=1.4)
    s.save()


def herdr():
    """In herdr: Ctrl-B m opens the picker over the pane; Enter resumes in the shell underneath.
    A herdr of its own: the demo home holds its config, with only the sessift plugin linked."""
    config = os.path.join(HOME, ".config", "herdr")
    shutil.rmtree(config, ignore_errors=True)  # no workspaces restored from an earlier run
    os.makedirs(config, exist_ok=True)
    open(os.path.join(config, "config.toml"), "w").write(
        'onboarding = false\n\n[update]\nversion_check = false\nmanifest_check = false\n\n'
        '[[keys.command]]\nkey = "prefix+m"\ntype = "plugin_action"\ncommand = "sessift.open"\n'
        'description = "搜索并恢复 agent 会话"\n')
    plugin = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "herdr")
    subprocess.run([os.path.join(bindir, "herdr"), "plugin", "link", os.path.abspath(plugin)],
                   env=ENV, capture_output=True, check=True)
    start("herdr", project, cols=132, rows=32)
    time.sleep(3.0)
    s = Scene("herdr")
    s.shot([], "", 1.2)
    s.press("C-b", ["Ctrl-B"], "herdr 前缀键", wait=0.4, hold=0.6)
    s.press("m", ["Ctrl-B", "m"], "在任何 pane 上打开浮层", wait=2.8, hold=1.8)
    s.type("回滚", "边打字边搜")
    s.press("Down", ["↓"], "选会话", wait=0.8, hold=1.4)
    s.press("Enter", ["Enter"], "在下面空闲的 shell 里恢复（pane 忙就开新 tab）", wait=2.0, hold=3.4)
    s.save()
    subprocess.run([os.path.join(bindir, "herdr"), "server", "stop"], env=ENV, capture_output=True)


SCENES = {"search": search, "reader": reader, "herdr": herdr}
try:
    for name in sys.argv[4:] or list(SCENES):
        SCENES[name]()
finally:
    tmux("kill-server", check=False)
    subprocess.run([BIN, "broker", "stop"], env=ENV, capture_output=True)
    if os.path.exists(os.path.join(HOME, ".config", "herdr")):
        subprocess.run([os.path.join(bindir, "herdr"), "server", "stop"], env=ENV, capture_output=True)

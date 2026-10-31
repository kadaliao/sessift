#!/bin/sh
# Regenerate the README GIFs (docs/images/*.gif) from fictional sessions on two pretend
# machines. Runs the real TUI, and a private herdr, in an isolated tmux server with one capture
# per key press, then renders the frames with headless Chrome; the keys pressed are shown under
# the window.
#
# Needs: tmux, herdr, Google Chrome, ffmpeg, Python 3 with Playwright, and the
# "Maple Mono NF CN" font.
set -eu
cd "$(dirname "$0")/../.."
cargo build --release
bin="$PWD/target/release/sessift"
images="$PWD/docs/images"
mkdir -p "$images"
# A short path: the broker's Unix socket lives under it (at most 104 bytes).
work=$(mktemp -d /tmp/sessift-gif.XXXXXX)
trap 'rm -rf "$work"' EXIT
python3 scripts/screenshots/demo.py "$work/demo" "$bin"
python3 scripts/screenshots/record.py "$work/demo" "$bin" "$work/out"
python3 scripts/screenshots/gif.py "$work/out" search "$images/search.gif" "~/work/shop-api"
python3 scripts/screenshots/gif.py "$work/out" reader "$images/reader.gif"
python3 scripts/screenshots/gif.py "$work/out" herdr "$images/herdr.gif" herdr

#!/bin/sh
# Regenerate the README screenshots (docs/images/*.png) from fictional sessions on two
# pretend machines. Runs the real TUI in a private tmux server, captures the screens and
# renders them with headless Chrome.
#
# Needs: tmux, Google Chrome, ImageMagick (`magick`), Python 3 with Pillow, and the
# "Maple Mono NF CN" font.
set -eu
cd "$(dirname "$0")/../.."
cargo build --release
bin="$PWD/target/release/sessift"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
python3 scripts/screenshots/demo.py "$work/demo" "$bin"
python3 scripts/screenshots/shoot.py "$work/demo" "$bin" "$work/out"
for name in search menu reader; do
    python3 scripts/screenshots/render.py "$work/out/$name.ansi" "docs/images/$name.png" sessift
done

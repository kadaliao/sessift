#!/bin/sh
# The `open` action: show the picker pane, which the manifest places in a popup.
exec "${HERDR_BIN_PATH:-herdr}" plugin pane open \
  --plugin "${HERDR_PLUGIN_ID:-sessift}" --entrypoint picker >/dev/null

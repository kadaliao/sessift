#!/bin/sh
# Runs inside the popup. `sessift pick --herdr` resumes the chosen session through Herdr:
# the pane already running it, the idle shell under the popup, or a new tab.

pause() {
  printf '\n按回车关闭'
  read -r _
}

bin=$(command -v sessift 2>/dev/null)
if [ -z "$bin" ]; then
  # The last one is where `sessift remote install` puts it on other machines.
  for candidate in "$HOME/.local/bin/sessift" /opt/homebrew/bin/sessift \
    /usr/local/bin/sessift "$HOME/.cargo/bin/sessift" "$HOME/.sessift/bin/sessift"; do
    if [ -x "$candidate" ]; then
      bin=$candidate
      break
    fi
  done
fi
if [ -z "$bin" ]; then
  echo "找不到 sessift。安装：brew install kadaliao/tap/sessift"
  echo "或者从 https://github.com/kadaliao/sessift/releases 下载。"
  pause
  exit 1
fi

"$bin" pick --herdr
status=$?
if [ "$status" -ne 0 ]; then
  # clap exits with 2 on an unknown flag: a sessift from before the Herdr plugin.
  if [ "$status" -eq 2 ]; then
    echo "这个 sessift（$bin）不支持 Herdr 插件，请升级到最新版本。"
  fi
  pause
fi
exit "$status"

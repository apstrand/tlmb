#!/bin/bash
# Photograph the compositor. Falls back to a TUI pane dump if grim cannot.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

out=${1:?usage: capture.sh <path.png>}
mkdir -p "$(dirname "$out")"

if command -v grim >/dev/null && [[ -n ${WAYLAND_DISPLAY:-} ]]; then
  grim "$out"
  echo "$out"
  exit 0
fi

echo "grim unavailable; compositor not up?" >&2
exit 1

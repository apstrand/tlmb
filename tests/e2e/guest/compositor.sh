#!/bin/bash
# Start (or reuse) a headless Sway. GUI and Chromium need a live Wayland
# display; grim photographs it. A leftover socket from a dead compositor
# is not "already up".
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

mkdir -p "$HOME/.config/sway"
cp "$HERE/sway.conf" "$HOME/.config/sway/config"

sway_alive() {
  local pid
  pid=$(cat /tmp/mailbox-e2e-sway.pid 2>/dev/null) || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  [[ -n ${WAYLAND_DISPLAY:-} && -S ${XDG_RUNTIME_DIR}/$WAYLAND_DISPLAY ]]
}

if sway_alive; then
  echo "compositor already up on $WAYLAND_DISPLAY"
  exit 0
fi

# A leftover compositor holds the socket; winit then dies with "Connection reset".
if [[ -f /tmp/mailbox-e2e-sway.pid ]]; then
  kill "$(cat /tmp/mailbox-e2e-sway.pid)" 2>/dev/null || true
  sleep 0.2
fi
pkill -u "$(id -un)" -x sway 2>/dev/null || true
rm -f "$XDG_RUNTIME_DIR"/wayland-* "$XDG_RUNTIME_DIR"/sway-ipc.*
unset WAYLAND_DISPLAY

export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER="${WLR_RENDERER:-pixman}"
sway -c "$HERE/sway.conf" >/tmp/mailbox-e2e-sway.log 2>&1 &
echo $! >/tmp/mailbox-e2e-sway.pid

for _ in $(seq 50); do
  if [[ -f /tmp/mailbox-e2e-sway.pid ]] && ! kill -0 "$(cat /tmp/mailbox-e2e-sway.pid)" 2>/dev/null; then
    break
  fi
  # shellcheck disable=SC1091
  source "$HERE/session-env.sh"
  if sway_alive; then
    echo "compositor ready on $WAYLAND_DISPLAY"
    exit 0
  fi
  sleep 0.1
done
echo "compositor failed to start:" >&2
cat /tmp/mailbox-e2e-sway.log >&2 || true
exit 1

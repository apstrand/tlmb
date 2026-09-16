#!/bin/bash
# The desktop window (Tauri/WebKitGTK) on the headless compositor.
#
#   gui.sh [scenario]
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" "$SCENARIO"
"$HERE/compositor.sh"
# shellcheck disable=SC1091
source "$HERE/session-env.sh"
export GDK_BACKEND=wayland
export WINIT_UNIX_BACKEND=wayland
mailbox gui >/tmp/mailbox-e2e-gui.log 2>&1 &
echo $! >/tmp/mailbox-e2e-gui.pid
for _ in $(seq 25); do
  kill -0 "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || break
  sleep 0.2
done
if ! kill -0 "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null; then
  echo "mailbox gui exited:" >&2
  cat /tmp/mailbox-e2e-gui.log >&2
  exit 1
fi
# WebKitGTK needs a beat to paint on pixman; grim of an empty output is a false pass.
sleep 2
if ! kill -0 "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null; then
  echo "mailbox gui exited before screenshot:" >&2
  cat /tmp/mailbox-e2e-gui.log >&2
  exit 1
fi
shot "gui-${SCENARIO}"
kill "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || true
wait "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || true
echo "gui $SCENARIO ok"

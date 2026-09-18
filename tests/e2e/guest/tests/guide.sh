#!/bin/bash
# Drive every screen used by docs/USER-GUIDE.md and copy out deterministic PNGs.
set -euo pipefail
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

need chromium
need python3
"$HERE/compositor.sh"
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

token=e2e-guide-token
port=38895
url="http://127.0.0.1:$port/?mailbox_token=$token"

stop_apps() {
  kill "$(cat /tmp/mailbox-e2e-chromium.pid 2>/dev/null)" 2>/dev/null || true
  kill "$(cat /tmp/mailbox-e2e-serve.pid 2>/dev/null)" 2>/dev/null || true
}
trap 'stop_apps; stop_recording' EXIT

start_apps() {
  local profile=$1
  mailbox serve --port "$port" --token "$token" --bind 127.0.0.1 \
    >/tmp/mailbox-e2e-serve.log 2>&1 &
  echo $! >/tmp/mailbox-e2e-serve.pid
  wait_for 80 curl -fsS -o /dev/null "$url" || {
    cat /tmp/mailbox-e2e-serve.log >&2
    exit 1
  }
  rm -rf "$profile"
  chromium --ozone-platform=wayland --disable-gpu --no-first-run \
    --no-default-browser-check --disable-dev-shm-usage --password-store=basic \
    --use-mock-keychain --disable-features=Translate \
    --user-data-dir="$profile" --window-size=1280,900 \
    --remote-debugging-port=9222 --remote-allow-origins='*' "$url" \
    >/tmp/mailbox-e2e-chromium.log 2>&1 &
  echo $! >/tmp/mailbox-e2e-chromium.pid
}

# First launch: no config, and therefore no accidental dependence on a seeded account.
rm -f "$MAILBOX_CONFIG"
rm -rf "$MAILBOX_STATE_DIR"
start_apps /tmp/mailbox-e2e-guide-setup
python3 "$E2E_TESTS/guide.py" setup "$url" "$EVIDENCE" "$token"
stop_apps
sleep 0.5

# The normal mailbox. The separate seeding path keeps the setup screenshot free
# of a fake successful account while letting the rest use the standard fixture.
"$HERE/seed.sh" inbox
start_apps /tmp/mailbox-e2e-guide-mail
python3 "$E2E_TESTS/guide.py" mail "$url" "$EVIDENCE" "$token" \
  >"$EVIDENCE/guide-checks.log" 2>&1
cat "$EVIDENCE/guide-checks.log"

for image in setup inbox folders compose shortcuts; do
  [[ -s "$EVIDENCE/guide-$image.png" ]] || { echo "missing guide-$image.png" >&2; exit 1; }
done
echo "guide ok"

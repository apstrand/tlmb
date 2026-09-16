#!/bin/bash
# §7's folder list, driven rather than photographed: the count is unread
# mail, the hover has both numbers, and on a phone the panes are a stack the
# back arrow walks up. A screenshot cannot show a click that does nothing,
# so this one clicks.
#
#   folders.sh [scenario]
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" "$SCENARIO"
need chromium
need python3
need jq
"$HERE/compositor.sh"
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

token=e2e-ui-token
port=38895
mailbox serve --port "$port" --token "$token" --bind 127.0.0.1 \
  >/tmp/mailbox-e2e-serve.log 2>&1 &
echo $! >/tmp/mailbox-e2e-serve.pid
url="http://127.0.0.1:$port/?mailbox_token=$token"
if ! wait_for 80 curl -fsS -o /dev/null "$url"; then
  echo "serve did not come up" >&2
  cat /tmp/mailbox-e2e-serve.log >&2
  exit 1
fi
api_has_mail() {
  curl -fsS -H "X-Mailbox-Token: $token" \
    "http://127.0.0.1:$port/api/ls?limit=200" | jq -e '.messages | length > 0' >/dev/null
}
if ! wait_for 50 api_has_mail; then
  echo "serve came up but /api/ls is empty" >&2
  cat /tmp/mailbox-e2e-serve.log >&2
  exit 1
fi

# Headed, on the compositor, so the recording has something to show. The
# debugger port is the only way in: chromium's own flags cannot click.
rm -rf /tmp/mailbox-e2e-chrome
chromium --ozone-platform=wayland --disable-gpu --no-first-run \
  --no-default-browser-check --disable-dev-shm-usage --password-store=basic \
  --use-mock-keychain --disable-features=Translate \
  --user-data-dir=/tmp/mailbox-e2e-chrome --window-size=1280,720 \
  --remote-debugging-port=9222 --remote-allow-origins='*' "$url" \
  >/tmp/mailbox-e2e-chromium.log 2>&1 &
echo $! >/tmp/mailbox-e2e-chromium.pid

status=0
python3 "$E2E_TESTS/folders.py" "$url" "$EVIDENCE" "$token" \
  >"$EVIDENCE/folders-${SCENARIO}.log" 2>&1 || status=$?
cat "$EVIDENCE/folders-${SCENARIO}.log"

stop_recording
kill "$(cat /tmp/mailbox-e2e-chromium.pid)" 2>/dev/null || true
kill "$(cat /tmp/mailbox-e2e-serve.pid)" 2>/dev/null || true
cp /tmp/mailbox-e2e-serve.log "$EVIDENCE/folders-${SCENARIO}-serve.log" 2>/dev/null || true
cp /tmp/mailbox-e2e-chromium.log "$EVIDENCE/folders-${SCENARIO}-chromium.log" 2>/dev/null || true
if ((status != 0)); then
  echo "folders $SCENARIO FAILED" >&2
  tail -n 40 /tmp/mailbox-e2e-chromium.log >&2 || true
  exit "$status"
fi
echo "folders $SCENARIO ok"

#!/bin/bash
# The served page, photographed: chromium renders it once and dumps the DOM.
#
#   web.sh [scenario]
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" "$SCENARIO"
need chromium
need jq
token=e2e-ui-token
mailbox serve --port 38895 --token "$token" --bind 127.0.0.1 >/tmp/mailbox-e2e-serve.log 2>&1 &
echo $! >/tmp/mailbox-e2e-serve.pid
if ! wait_for 80 curl -fsS -o /dev/null "http://127.0.0.1:38895/?mailbox_token=$token"; then
  echo "serve did not come up" >&2
  cat /tmp/mailbox-e2e-serve.log >&2
  exit 1
fi
# The page fetches /api/ls after onload. A screenshot of the chrome with an
# empty list is a false pass — wait until the same contract the UI calls
# actually has rows, then give Chromium virtual time to paint them.
api_has_mail() {
  curl -fsS -H "X-Mailbox-Token: $token" \
    "http://127.0.0.1:38895/api/ls?limit=200" \
    | jq -e '.messages | length > 0' >/dev/null
}
if ! wait_for 50 api_has_mail; then
  echo "serve came up but /api/ls is empty" >&2
  curl -sS -H "X-Mailbox-Token: $token" "http://127.0.0.1:38895/api/ls?limit=200" >&2 || true
  cat /tmp/mailbox-e2e-serve.log >&2 || true
  exit 1
fi
url="http://127.0.0.1:38895/?mailbox_token=$token"
chromium --headless=new --disable-gpu --no-first-run --window-size=1280,800 \
  --disable-dev-shm-usage --run-all-compositor-stages-before-draw \
  --virtual-time-budget=12000 \
  --screenshot="$EVIDENCE/web-${SCENARIO}.png" \
  --dump-dom "$url" \
  >"$EVIDENCE/web-${SCENARIO}.html" 2>/tmp/mailbox-e2e-chromium.log || true
[[ -s $EVIDENCE/web-${SCENARIO}.png ]] || {
  echo "chromium produced no screenshot" >&2
  cat /tmp/mailbox-e2e-chromium.log >&2
  exit 1
}
case "$SCENARIO" in
inbox) expect_file_has "$EVIDENCE/web-${SCENARIO}.html" "Invoice 42" ;;
html-images) expect_file_has "$EVIDENCE/web-${SCENARIO}.html" "Your order has shipped" ;;
esac
kill "$(cat /tmp/mailbox-e2e-serve.pid)" 2>/dev/null || true
echo "web $SCENARIO ok"

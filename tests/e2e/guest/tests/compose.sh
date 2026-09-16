#!/bin/bash
# Mail out through the CLI into the loopback sink — never the internet.
#
#   compose.sh [scenario]
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" inbox
start_sink
# Queue through the CLI — the same path the windows use — then prove the
# sink, not the internet, has the bytes.
mailbox send --from peter@example.com --to alice@example.com \
  --subject "e2e compose" --body "hello from the isolated guest" --yes
mailbox outbox flush --yes || mailbox outbox flush || true
sleep 0.5
ls /tmp/mailbox-e2e-sent/*.eml >/dev/null 2>&1 \
  || { echo "smtp sink received nothing" >&2; cat /tmp/mailbox-e2e-sink.log >&2; exit 1; }
grep -F -q "hello from the isolated guest" /tmp/mailbox-e2e-sent/*.eml \
  || { echo "sunk mail did not contain the body" >&2; exit 1; }
echo "compose ok"

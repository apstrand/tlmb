#!/bin/bash
# The third belt: tcpdump while HTML mail with remote images is on screen.
#
#   isolation.sh [scenario]
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/probe-isolation.sh"
"$HERE/seed.sh" html-images
start_sink
sudo rm -f /tmp/mailbox-e2e-pcap.pcap
sudo timeout 8 tcpdump -i any -n -w /tmp/mailbox-e2e-pcap.pcap \
  'tcp and not host 127.0.0.1 and not host ::1' >/tmp/mailbox-e2e-tcpdump.log 2>&1 &
dump=$!
sleep 0.5
mailbox show "$(mailbox --json ls --fields id,subject | jq -r '.messages[] | select(.subject|test("shipped")) | .id')" \
  --format html >/tmp/mailbox-e2e-show.html || true
# Opening in the GUI is the leak vector: remote <img> would SYN out.
if "$HERE/compositor.sh"; then
  source "$HERE/session-env.sh"
  mailbox gui >/tmp/mailbox-e2e-gui.log 2>&1 &
  echo $! >/tmp/mailbox-e2e-gui.pid
  sleep 3
  shot "gui-html-images-isolation"
  kill "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || true
fi
sleep 1
sudo kill "$dump" 2>/dev/null || true
wait "$dump" 2>/dev/null || true
# A pcap with SYN to a public address is a leak.
if command -v tcpdump >/dev/null && [[ -s /tmp/mailbox-e2e-pcap.pcap ]]; then
  if tcpdump -nr /tmp/mailbox-e2e-pcap.pcap 2>/dev/null | grep -E 'tracker\.example|cdn\.shop\.example|1\.1\.1\.1' ; then
    echo "isolation FAIL: outbound packets while reading HTML mail" >&2
    tcpdump -nr /tmp/mailbox-e2e-pcap.pcap >&2 || true
    exit 1
  fi
fi
echo "isolation ok"

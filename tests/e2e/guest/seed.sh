#!/bin/bash
# Fill the local store (and optionally Dovecot) from a named scenario.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)
ROOT=${MAILBOX_E2E_ROOT:-$HOME/mailbox}
SCEN=${ROOT}/tests/e2e/scenarios
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

scenario=${1:-inbox}
mkdir -p "$(dirname "$MAILBOX_CONFIG")" "$MAILBOX_STATE_DIR"

cat >"$MAILBOX_CONFIG" <<TOML
[accounts.e2e]
address         = "peter@example.com"
imap_host       = "127.0.0.1"
imap_port       = 143
imap_security   = "none"
require_tls     = false
smtp_host       = "127.0.0.1"
smtp_port       = 2525
smtp_security   = "none"
offline         = "recent"

[general]
unified_inbox   = true
TOML

MAILBOX=${MAILBOX:-$HOME/bin/mailbox}
[[ -x $MAILBOX ]] || MAILBOX=$ROOT/target/release/mailbox
[[ -x $MAILBOX ]] || MAILBOX=$ROOT/target/debug/mailbox
[[ -x $MAILBOX ]] || { echo "no mailbox binary" >&2; exit 1; }

export PATH="$(dirname "$MAILBOX"):$PATH"

rm -rf "$MAILBOX_STATE_DIR"
mkdir -p "$MAILBOX_STATE_DIR"

case "$scenario" in
inbox | html-images | attachments | thread)
  manifest="$SCEN/${scenario}.toml"
  [[ -f $manifest ]] || { echo "no fixture $manifest" >&2; exit 1; }
  mailbox debug fixture load "$manifest"
  # Also push onto Dovecot so a later sync is a no-op rather than a wipe,
  # and so "sync" in the UI has a real server to talk to.
  mailbox debug fixture render "$manifest" \
    | python3 "$HERE/imap-append.py" --user e2e --password testpass
  ;;
large)
  n=${2:-3000}
  mailbox --yes debug generate --messages "$n" --seed 1
  ;;
paging)
  # Numbered so a test can read the cursor and the scroll off the screen:
  # "Paging 001" is the newest of 120, and the oldest is a 200-line letter
  # whose lines are "line 001" onwards.
  manifest=$(mktemp --suffix=.toml)
  python3 - "$manifest" <<'PY'
import json, sys
from datetime import datetime, timedelta, timezone
base = datetime(2026, 8, 28, 12, tzinfo=timezone.utc)
out = ['[[folder]]\npath = "INBOX"\nspecial_use = "inbox"\n']
def message(name, subject, when, body):
    out.append("\n".join([
        "[[message]]",
        f"name = {json.dumps(name)}",
        'folder = "INBOX"',
        'from = "Pat <pat@example.com>"',
        'to = ["peter@example.com"]',
        f"subject = {json.dumps(subject)}",
        f'date = "{when:%Y-%m-%dT%H:%M:%SZ}"',
        f'message_id = "<{name}@example.com>"',
        'flags = ["\\\\Seen"]',
        f"body = {json.dumps(body)}",
        "",
    ]))
for i in range(1, 121):
    message(f"p{i:03}", f"Paging {i:03}", base - timedelta(hours=i), f"Message {i}.\n")
message("long", "Long letter", base - timedelta(days=30),
        "".join(f"line {i:03} of the long letter\n" for i in range(1, 201)))
open(sys.argv[1], "w").write("\n".join(out))
PY
  mailbox debug fixture load "$manifest"
  rm -f "$manifest"
  ;;
imap)
  manifest="$SCEN/inbox.toml"
  mailbox debug fixture render "$manifest" \
    | python3 "$HERE/imap-append.py" --user e2e --password testpass
  mailbox sync --once --yes || mailbox sync --once
  ;;
*)
  echo "unknown scenario: $scenario (inbox|html-images|attachments|thread|large|paging|imap)" >&2
  exit 1
  ;;
esac

mailbox ls || true
echo "seeded $scenario"

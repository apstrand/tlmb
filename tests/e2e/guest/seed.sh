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
imap)
  manifest="$SCEN/inbox.toml"
  mailbox debug fixture render "$manifest" \
    | python3 "$HERE/imap-append.py" --user e2e --password testpass
  mailbox sync --once --yes || mailbox sync --once
  ;;
*)
  echo "unknown scenario: $scenario (inbox|html-images|attachments|thread|large|imap)" >&2
  exit 1
  ;;
esac

mailbox ls || true
echo "seeded $scenario"

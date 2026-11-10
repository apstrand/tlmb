#!/bin/bash
# Opening a message whose body was never downloaded fetches it (§5.1): the
# store keeps headers only for mail outside the prefetch window, and the
# terminal used to show "[body not downloaded]" for it forever.
#
#   tui-body.sh [scenario]      # scenario is ignored; it always seeds imap
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

# From the server by a real sync, so the store knows where each body lives.
"$HERE/seed.sh" imap
need tmux

# Headers only, as a sync leaves mail older than the prefetch window. The
# server keeps its copy, which is where the body has to come from.
python3 - "$MAILBOX_STATE_DIR" <<'PY'
import pathlib, sqlite3, sys
dbs = list(pathlib.Path(sys.argv[1]).glob("*/meta.db"))
assert dbs, "no store to strip"
for db in dbs:
    conn = sqlite3.connect(db)
    conn.execute("UPDATE messages SET blob_id = NULL")
    conn.commit()
    conn.close()
PY

PANE=mailbox-tui
trap 'tmux kill-session -t "$PANE" 2>/dev/null || true' EXIT
pane() { tmux capture-pane -t "$PANE" -p; }
wait_pane() {
  local needle=$1 name=$2
  for _ in $(seq 75); do
    pane | grep -F -q -- "$needle" && return 0
    sleep 0.2
  done
  tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-body-${name}-timeout.txt"
  echo "timed out waiting for '$needle' on screen:" >&2
  pane >&2
  exit 1
}

tmux kill-session -t "$PANE" 2>/dev/null || true
tmux new-session -d -s "$PANE" -x 120 -y 40
# No automatic sync: its prefetch would put the body back before the reader
# had to ask for it, and this is about the reader asking.
tmux send-keys -t "$PANE" "export MAILBOX_CONFIG=$MAILBOX_CONFIG MAILBOX_STATE_DIR=$MAILBOX_STATE_DIR MAILBOX_PASSWORD_E2E=testpass MAILBOX_NOTIFIER=none MAILBOX_CLOCK=$MAILBOX_CLOCK MAILBOX_AUTO_SYNC=off" Enter
tmux send-keys -t "$PANE" "mailbox tui" Enter
wait_pane "Invoice 42" start

# Whichever message is at the top; any of the inbox fixture's bodies will do.
tmux send-keys -t "$PANE" Enter
bodies='Please pay the invoice by Friday.|Paid, thanks.|The usual place at 12:30?|Nothing urgent this week.'
for _ in $(seq 75); do
  pane | grep -E -q -- "$bodies" && break
  sleep 0.2
done
tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-body.txt"
if ! pane | grep -E -q -- "$bodies"; then
  echo "the body never arrived:" >&2
  pane >&2
  exit 1
fi
tmux send-keys -t "$PANE" q q
echo "tui-body ok"

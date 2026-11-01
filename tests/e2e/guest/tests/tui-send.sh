#!/bin/bash
# Sending mail from the terminal interface, end to end: `m` and `r` hand the
# terminal to $EDITOR, the message is queued, `@` syncs, and the sync flushes
# the outbox into the loopback SMTP sink — never the internet.
#
#   tui-send.sh [scenario]      # scenario is ignored; it always seeds inbox
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" inbox
need tmux
start_sink
SENT=/tmp/mailbox-e2e-sent
mkdir -p "$SENT"
rm -f "$SENT"/*.eml

# Stands in for the person at $EDITOR: fills in what the template left blank
# and writes a body, the way someone would before saving and quitting.
EDITOR_SCRIPT=/tmp/mailbox-e2e-editor.sh
cat >"$EDITOR_SCRIPT" <<'SH'
#!/bin/sh
sed -i -e 's/^To: *$/To: alice@example.com/' \
  -e 's/^Subject: *$/Subject: e2e tui send/' "$1"
printf '%s\n' "${MAILBOX_E2E_BODY:-hello from the tui}" >>"$1"
SH
chmod +x "$EDITOR_SCRIPT"

PANE=mailbox-tui
# A failed run must not leave the TUI holding the binary the next sync copies.
trap 'tmux kill-session -t "$PANE" 2>/dev/null || true; stop_recording' EXIT
pane() { tmux capture-pane -t "$PANE" -p; }
# Waits for text on screen; on timeout keeps the screen as evidence and fails.
wait_pane() {
  local needle=$1 name=$2
  for _ in $(seq 75); do
    pane | grep -F -q -- "$needle" && return 0
    sleep 0.2
  done
  tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-send-${name}-timeout.txt"
  echo "timed out waiting for '$needle' on screen:" >&2
  pane >&2
  exit 1
}
sent_count() { find "$SENT" -name '*.eml' | wc -l; }
wait_sent() {
  local want=$1
  for _ in $(seq 75); do
    (($(sent_count) >= want)) && return 0
    sleep 0.2
  done
  echo "smtp sink has $(sent_count) message(s), wanted $want" >&2
  cat /tmp/mailbox-e2e-sink.log >&2
  exit 1
}

tmux kill-session -t "$PANE" 2>/dev/null || true
tmux new-session -d -s "$PANE" -x 120 -y 40
tmux send-keys -t "$PANE" "export MAILBOX_CONFIG=$MAILBOX_CONFIG MAILBOX_STATE_DIR=$MAILBOX_STATE_DIR MAILBOX_PASSWORD_E2E=testpass MAILBOX_NOTIFIER=none MAILBOX_CLOCK=$MAILBOX_CLOCK EDITOR=$EDITOR_SCRIPT" Enter
tmux send-keys -t "$PANE" "mailbox tui" Enter
wait_pane "Invoice 42" start
tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-send-1-start.txt"

# 1. Compose a new message with `m`.
tmux send-keys -t "$PANE" m
wait_pane "composed" composed
# The editor had the terminal; coming back has to repaint the list, not just
# the status line under a blank screen.
wait_pane "Invoice 42" composed-repaint
tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-send-2-composed.txt"

# 2. Reply with `r` to the message under the cursor.
tmux send-keys -t "$PANE" r
wait_pane "reply queued" reply
wait_pane "Invoice 42" reply-repaint
tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-send-3-replied.txt"

# Queued, not sent: the outbox is flushed by a sync, so nothing has left yet.
(($(sent_count) == 0)) || { echo "mail left before the sync" >&2; exit 1; }

# 3. `@` syncs, and the sync flushes the outbox: both go out.
tmux send-keys -t "$PANE" @
wait_sent 2
for _ in $(seq 50); do pane | grep -F -q "syncing" || break; sleep 0.2; done
tmux capture-pane -t "$PANE" -pe >"$EVIDENCE/tui-send-4-synced.txt"

composed=$(grep -l -F "Subject: e2e tui send" "$SENT"/*.eml | head -1 || true)
[[ -n $composed ]] || { echo "the composed mail is not in the sink" >&2; cat "$SENT"/*.eml >&2; exit 1; }
cp "$composed" "$EVIDENCE/tui-send-compose.eml"
expect_file_has "$composed" "hello from the tui"
grep -E -q '^To: .*alice@example.com' "$composed" \
  || { echo "composed mail is not addressed to alice" >&2; cat "$composed" >&2; exit 1; }
grep -E -q '^From: .*peter@example.com' "$composed" \
  || { echo "composed mail is not from the account" >&2; cat "$composed" >&2; exit 1; }

reply=$(grep -l -E '^In-Reply-To:' "$SENT"/*.eml | head -1 || true)
[[ -n $reply ]] || { echo "no sunk mail carries In-Reply-To" >&2; cat "$SENT"/*.eml >&2; exit 1; }
cp "$reply" "$EVIDENCE/tui-send-reply.eml"
grep -E -q '^Subject: Re: ' "$reply" \
  || { echo "reply subject is not 'Re: …'" >&2; cat "$reply" >&2; exit 1; }
expect_file_has "$reply" "hello from the tui"

tmux send-keys -t "$PANE" q
sleep 0.3
tmux kill-session -t "$PANE" 2>/dev/null || true
echo "tui-send ok: $(sent_count) message(s) reached the sink"

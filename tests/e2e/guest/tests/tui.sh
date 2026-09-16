#!/bin/bash
# The terminal interface, in tmux for the text and in foot for the picture.
#
#   tui.sh [scenario]
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" "$SCENARIO"
need tmux
tmux kill-session -t mailbox-tui 2>/dev/null || true
tmux new-session -d -s mailbox-tui -x 120 -y 40
tmux send-keys -t mailbox-tui "export MAILBOX_CONFIG=$MAILBOX_CONFIG MAILBOX_STATE_DIR=$MAILBOX_STATE_DIR MAILBOX_PASSWORD_E2E=testpass MAILBOX_NOTIFIER=none MAILBOX_CLOCK=$MAILBOX_CLOCK" Enter
tmux send-keys -t mailbox-tui "mailbox tui" Enter
sleep 1.5
tmux capture-pane -t mailbox-tui -pe >"$EVIDENCE/tui-${SCENARIO}.txt"
"$HERE/compositor.sh"
# shellcheck disable=SC1091
source "$HERE/session-env.sh"
foot -a mailbox-tui -e env MAILBOX_CONFIG="$MAILBOX_CONFIG" \
  MAILBOX_STATE_DIR="$MAILBOX_STATE_DIR" MAILBOX_PASSWORD_E2E=testpass \
  MAILBOX_NOTIFIER=none MAILBOX_CLOCK="$MAILBOX_CLOCK" mailbox tui \
  >/tmp/mailbox-e2e-foot.log 2>&1 &
echo $! >/tmp/mailbox-e2e-foot.pid
sleep 1.5
shot "tui-${SCENARIO}"
kill "$(cat /tmp/mailbox-e2e-foot.pid)" 2>/dev/null || true
tmux send-keys -t mailbox-tui q
sleep 0.3
tmux kill-session -t mailbox-tui 2>/dev/null || true
case "$SCENARIO" in
inbox) expect_file_has "$EVIDENCE/tui-${SCENARIO}.txt" "Invoice 42" ;;
html-images) expect_file_has "$EVIDENCE/tui-${SCENARIO}.txt" "Your order has shipped" ;;
attachments) expect_file_has "$EVIDENCE/tui-${SCENARIO}.txt" "Invoice attached" ;;
thread) expect_file_has "$EVIDENCE/tui-${SCENARIO}.txt" "The parser rewrite" ;;
large) expect_file_has "$EVIDENCE/tui-${SCENARIO}.txt" "id" ;;
esac
echo "tui $SCENARIO ok"

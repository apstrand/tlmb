#!/bin/bash
# Drive one UI against a seeded mailbox and write evidence under $EVIDENCE.
#
#   run.sh tui|web|gui|folders|compose|isolation [scenario]
#
# Exits non-zero if a screenshot is empty or a TUI pane is missing expected text.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)
ROOT=${MAILBOX_E2E_ROOT:-$HOME/mailbox}
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

MAILBOX=${MAILBOX:-$HOME/bin/mailbox}
[[ -x $MAILBOX ]] || MAILBOX=$ROOT/target/release/mailbox
[[ -x $MAILBOX ]] || MAILBOX=$ROOT/target/debug/mailbox
export PATH="$(dirname "$MAILBOX"):$PATH"

UI=${1:?usage: run.sh tui|web|gui|folders|compose|isolation [scenario]}
SCENARIO=${2:-inbox}
EVIDENCE=${EVIDENCE:-$HOME/mailbox-e2e-out}
mkdir -p "$EVIDENCE"

need() { command -v "$1" >/dev/null || { echo "missing $1" >&2; exit 1; }; }

VIDEO=$EVIDENCE/${UI}-${SCENARIO}.mp4
RECORDING=no

# `mailbox-e2e check` asks for video; a bare `run` stays quiet and quick.
# Recording is per-run rather than per-case so every UI with a compositor gets
# it, and the trap stops it on a failed run too — a video of the failure is
# the one worth keeping.
start_recording() {
  [[ ${MAILBOX_E2E_VIDEO:-0} == 1 ]] || return 0
  case "$UI" in tui | gui | folders | isolation) ;; *) return 0 ;; esac
  "$HERE/compositor.sh" >/dev/null 2>&1 || return 0
  # shellcheck disable=SC1091
  source "$HERE/session-env.sh"
  if "$HERE/record.sh" start "$VIDEO" >/tmp/mailbox-e2e-record-start.log 2>&1; then
    RECORDING=yes
  else
    echo "no recorder in the guest; continuing without video" >&2
  fi
}

stop_recording() {
  [[ $RECORDING == yes ]] || return 0
  RECORDING=no
  "$HERE/record.sh" stop "$VIDEO" >/dev/null 2>&1 || true
}

trap stop_recording EXIT

# A previous scenario's gui/serve/tui must not hold the port or the display.
for pidfile in /tmp/mailbox-e2e-gui.pid /tmp/mailbox-e2e-serve.pid \
               /tmp/mailbox-e2e-foot.pid /tmp/mailbox-e2e-chromium.pid; do
  if [[ -f $pidfile ]]; then
    kill "$(cat "$pidfile")" 2>/dev/null || true
    rm -f "$pidfile"
  fi
done
tmux kill-session -t mailbox-tui 2>/dev/null || true

wait_for() {
  local n=$1; shift
  for _ in $(seq "$n"); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  return 1
}

start_sink() {
  if ! [[ -f /tmp/mailbox-e2e-sink.pid ]] || ! kill -0 "$(cat /tmp/mailbox-e2e-sink.pid)" 2>/dev/null; then
    python3 "$HERE/smtp-sink.py" >/tmp/mailbox-e2e-sink.log 2>&1 &
    echo $! >/tmp/mailbox-e2e-sink.pid
    sleep 0.3
  fi
}

shot() {
  local name=$1
  "$HERE/capture.sh" "$EVIDENCE/${name}.png"
  [[ -s $EVIDENCE/${name}.png ]] || { echo "empty screenshot $name" >&2; exit 1; }
}

expect_file_has() {
  local file=$1 needle=$2
  grep -F -q -- "$needle" "$file" || {
    echo "expected '$needle' in $file:" >&2
    cat "$file" >&2
    exit 1
  }
}

start_recording

case "$UI" in
tui)
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
  ;;

web)
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
  ;;

folders)
  # §7's folder list, driven rather than photographed: the count is unread
  # mail, the hover has both numbers, and on a phone the panes are a stack the
  # back arrow walks up. A screenshot cannot show a click that does nothing,
  # so this one clicks.
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
  python3 "$HERE/drive-folders.py" "$url" "$EVIDENCE" "$token" \
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
  ;;

gui)
  "$HERE/seed.sh" "$SCENARIO"
  "$HERE/compositor.sh"
  # shellcheck disable=SC1091
  source "$HERE/session-env.sh"
  export GDK_BACKEND=wayland
  export WINIT_UNIX_BACKEND=wayland
  mailbox gui >/tmp/mailbox-e2e-gui.log 2>&1 &
  echo $! >/tmp/mailbox-e2e-gui.pid
  for _ in $(seq 25); do
    kill -0 "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || break
    sleep 0.2
  done
  if ! kill -0 "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null; then
    echo "mailbox gui exited:" >&2
    cat /tmp/mailbox-e2e-gui.log >&2
    exit 1
  fi
  # WebKitGTK needs a beat to paint on pixman; grim of an empty output is a false pass.
  sleep 2
  if ! kill -0 "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null; then
    echo "mailbox gui exited before screenshot:" >&2
    cat /tmp/mailbox-e2e-gui.log >&2
    exit 1
  fi
  shot "gui-${SCENARIO}"
  kill "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || true
  wait "$(cat /tmp/mailbox-e2e-gui.pid)" 2>/dev/null || true
  echo "gui $SCENARIO ok"
  ;;

compose)
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
  ;;

isolation)
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
  ;;

*)
  echo "usage: run.sh tui|web|gui|folders|compose|isolation [scenario]" >&2
  exit 1
  ;;
esac

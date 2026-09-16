#!/bin/bash
# What every test under guest/tests/ shares: the session, the binary, the
# helpers, and the recording. Sourced, never run.
#
#   source .../lib.sh        # from a test script, which passes it $1 as the
#                            # scenario and takes its own name as the UI
#
# After sourcing: $HERE is the guest scripts directory, $E2E_TESTS the tests
# beside it, $EVIDENCE where pictures go, $UI and $SCENARIO what is running,
# and a recorder is already rolling if `--video` asked for one.

HERE=$(cd -- "$(dirname -- "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)
E2E_TESTS="$HERE/tests"
ROOT=${MAILBOX_E2E_ROOT:-$HOME/mailbox}
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

MAILBOX=${MAILBOX:-$HOME/bin/mailbox}
[[ -x $MAILBOX ]] || MAILBOX=$ROOT/target/release/mailbox
[[ -x $MAILBOX ]] || MAILBOX=$ROOT/target/debug/mailbox
export PATH="$(dirname "$MAILBOX"):$PATH"
# `import cdp` from a driver in tests/, without every driver saying so.
export PYTHONPATH="$HERE${PYTHONPATH:+:$PYTHONPATH}"

# The test's own filename is the UI, so a script run directly and the same
# script run through `run.sh` name their evidence identically.
UI=${UI:-$(basename "$(readlink -f "$0")" .sh)}
SCENARIO=${1:-inbox}
EVIDENCE=${EVIDENCE:-$HOME/mailbox-e2e-out}
mkdir -p "$EVIDENCE"

need() { command -v "$1" >/dev/null || { echo "missing $1" >&2; exit 1; }; }

VIDEO=$EVIDENCE/${UI}-${SCENARIO}.mp4
RECORDING=no

# `mailbox-e2e --video` asks for a recording; without it a run stays quiet and
# quick. Recording lives here rather than in the tests so every UI with a
# compositor gets it, and the trap stops it on a failed run too — a video of
# the failure is the one worth keeping.
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

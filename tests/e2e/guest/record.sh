#!/bin/bash
# Start or stop a compositor recording. Evidence for bug reports.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)
# shellcheck disable=SC1091
source "$HERE/session-env.sh"

cmd=${1:-start}
out=${2:-/tmp/mailbox-e2e.mp4}
pidfile=/tmp/mailbox-e2e-record.pid

case "$cmd" in
start)
  "$HERE/compositor.sh"
  # shellcheck disable=SC1091
  source "$HERE/session-env.sh"
  [[ -n ${WAYLAND_DISPLAY:-} ]] || { echo "no compositor" >&2; exit 1; }
  mkdir -p "$(dirname "$out")"
  if command -v wf-recorder >/dev/null; then
    # Headless pixman has no dmabuf. -D so idle screens still produce frames.
    wf-recorder --no-dmabuf -D -r 10 -f "$out" >/tmp/mailbox-e2e-record.log 2>&1 &
    echo $! >"$pidfile"
    echo "recording $out (wf-recorder pid $!)"
  elif command -v ffmpeg >/dev/null; then
    # Frame-by-frame grim, then encode on stop. Heavier, always works.
    dir=$(mktemp -d /tmp/mailbox-e2e-frames.XXXX)
    echo "$dir" >/tmp/mailbox-e2e-record-frames
    echo "$out" >/tmp/mailbox-e2e-record-out
    (
      i=0
      while [[ -d $dir ]]; do
        grim "$(printf '%s/%04d.png' "$dir" "$i")" || break
        i=$((i + 1))
        sleep 0.1
      done
    ) >/tmp/mailbox-e2e-record.log 2>&1 &
    echo $! >"$pidfile"
    echo "recording frames in $dir (pid $!)"
  else
    echo "need wf-recorder or ffmpeg" >&2
    exit 1
  fi
  ;;
stop)
  if [[ -f $pidfile ]]; then
    kill "$(cat "$pidfile")" 2>/dev/null || true
    wait "$(cat "$pidfile")" 2>/dev/null || true
    rm -f "$pidfile"
  fi
  if [[ -f /tmp/mailbox-e2e-record-frames ]]; then
    dir=$(cat /tmp/mailbox-e2e-record-frames)
    dest=$(cat /tmp/mailbox-e2e-record-out)
    ffmpeg -y -framerate 10 -i "$dir/%04d.png" -pix_fmt yuv420p "$dest" \
      >/tmp/mailbox-e2e-record-encode.log 2>&1 || true
    rm -rf "$dir" /tmp/mailbox-e2e-record-frames /tmp/mailbox-e2e-record-out
    echo "$dest"
  else
    echo "$out"
  fi
  ;;
*)
  echo "usage: record.sh start|stop [path.mp4]" >&2
  exit 1
  ;;
esac

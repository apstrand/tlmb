#!/bin/bash
# Fail if this guest can speak mail (or any TCP) to the world.
set -euo pipefail

fail() { echo "isolation FAIL: $*" >&2; exit 1; }

# Mail ports on anything but loopback must not connect.
for port in 25 465 587 143 993; do
  if timeout 2 bash -c "echo >/dev/tcp/1.1.1.1/$port" 2>/dev/null; then
    fail "tcp/1.1.1.1:$port is reachable"
  fi
done

# The in-guest servers must still answer on loopback.
timeout 2 bash -c "echo >/dev/tcp/127.0.0.1/143" 2>/dev/null \
  || fail "Dovecot is not listening on loopback:143"

echo "isolation ok: mail ports are loopback-only"

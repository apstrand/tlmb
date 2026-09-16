#!/bin/bash
# Run one of the tests in guest/tests/ against a seeded mailbox.
#
#   run.sh <ui> [scenario]
#
# A dispatcher, nothing more: each test is its own script under tests/ and can
# be run directly in the guest. What they share — the session, the binary, the
# helpers, the recorder — is in lib.sh.
set -euo pipefail
HERE=$(cd -- "$(dirname -- "$(readlink -f "$0")")" && pwd)

known() { (cd "$HERE/tests" && ls ./*.sh | sed 's|\./||; s|\.sh$||' | tr '\n' ' '); }

UI=${1:-}
SCENARIO=${2:-inbox}
script="$HERE/tests/$UI.sh"
if [[ -z $UI || ! -f $script ]]; then
  echo "usage: run.sh <ui> [scenario] — one of: $(known)" >&2
  exit 1
fi

exec bash "$script" "$SCENARIO"

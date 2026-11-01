#!/bin/bash
# Paging in the terminal interface: PageDown / PageUp and C-f / C-b, in the
# message list and in an open message.
#
#   tui-paging.sh [scenario]    # scenario is ignored; it always seeds paging
#
# Runnable on its own in the guest, or through `run.sh` / `mailbox-e2e run`.
set -euo pipefail
source "$(cd -- "$(dirname -- "$(readlink -f "$0")")/.." && pwd)/lib.sh"

"$HERE/seed.sh" paging
need tmux
python3 "$E2E_TESTS/tui_paging.py"

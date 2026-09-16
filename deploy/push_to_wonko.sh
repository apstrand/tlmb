#!/bin/bash
set -euo pipefail

# The shared root URL selects a configured upstream from each account's host/port.
relay_url=${1:-wss://relay.2615.io/}

top=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

target=wonko.nena.se:web/relay

cd "$top"

scp crates/mailbox-wasm/mailbox-relay.py "$target/"
ssh wonko.nena.se "systemctl --user restart mailbox-relay"

crates/mailbox-wasm/build-web.sh
cargo run -p mailbox-cli -- site --out dist --relay "$relay_url"

rsync -av dist/ wonko.nena.se:web/tlmb/

python crates/mailbox-wasm/deploy-check.py \
  --site https://tlmb.2615.io/ \
  --relay "$relay_url" \
  --imap-host mail.nena.se

#!/bin/sh
set -eu

cargo build -p mailbox-wasm --target wasm32-unknown-unknown
"${WASM_BINDGEN:-wasm-bindgen}" \
  --target web \
  --out-dir crates/mailbox-wasm/web/pkg \
  target/wasm32-unknown-unknown/debug/mailbox_wasm.wasm

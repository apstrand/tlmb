#!/bin/sh
# Builds the web interface into the bundle `mailbox serve` embeds (§3).
#
# Not a build.rs: cross-compiling from inside a build script means every
# ordinary `cargo build` needs the wasm target and wasm-bindgen installed, and
# fails confusingly for anyone who has neither. This is a separate, explicit
# step, and a binary built without it still serves — the page says the UI was
# not compiled in rather than showing a blank canvas.
#
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version <the one in Cargo.lock>
set -eu

root=$(cd "$(dirname "$0")" && pwd)
out="$root/crates/mailbox-web/web/dist"

version=$(sed -n '/^name = "wasm-bindgen"$/,/^version/ s/^version = "\(.*\)"/\1/p' "$root/Cargo.lock" | head -1)
have=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || echo "none")
if [ "$have" != "$version" ]; then
    echo "wasm-bindgen $have does not match the $version in Cargo.lock." >&2
    echo "The generated glue and the wasm module have to agree, so:" >&2
    echo "    cargo install wasm-bindgen-cli --version $version" >&2
    exit 1
fi

echo "building the web interface…"
cargo build --release -p mailbox-web-ui --target wasm32-unknown-unknown

mkdir -p "$out"
wasm-bindgen \
    --target web \
    --no-typescript \
    --out-dir "$out" \
    --out-name mailbox \
    "$root/target/wasm32-unknown-unknown/release/mailbox_web_ui.wasm"

size=$(wc -c < "$out/mailbox_bg.wasm")
echo "web interface: $((size / 1024)) KiB at $out"

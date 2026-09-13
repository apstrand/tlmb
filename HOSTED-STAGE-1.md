# Hosted flavour: Stage 1 report

Date: 2026-09-13

Verdict: **the blocking Worker transport works.** The native blocking IMAP
engine can run unchanged in a browser Worker while the page's main thread owns
and pumps the WebSocket.

## Implementation

- `mailbox_core::wasm_stream::BrowserStream` implements `Read` and `Write`
  over a two-slot control array and a 64 KiB data array backed by
  SharedArrayBuffer.
- The Worker blocks with `Atomics.wait`; the main thread remains responsive and
  services read/write requests from the WebSocket.
- `imap::connection::Transport` uses `BrowserStream` as its socket type on
  `wasm32`, including implicit TLS, STARTTLS upgrades, and read timeouts. Native
  builds continue to use `TcpStream`.
- The spike server emits `Cross-Origin-Opener-Policy: same-origin` and
  `Cross-Origin-Embedder-Policy: require-corp`, making the document
  cross-origin isolated as SharedArrayBuffer requires.

## Browser verification

Headless Chromium reported `crossOriginIsolated === true`. The page opened the
fixed-destination relay, started the wasm core in a module Worker, and called
the real blocking `mailbox_core::imap::Connection::connect` path. That path:

1. wrote the rustls client handshake through `BrowserStream`;
2. slept in the Worker while the main thread forwarded opaque TLS records;
3. verified Gmail's certificate;
4. read and parsed the IMAP greeting; and
5. returned the server capability set, including `IMAP4REV1`, `IDLE`, and
   `AUTH=XOAUTH2`.

## Verification

```text
cargo build -p mailbox-wasm --target wasm32-unknown-unknown     PASS
Chromium Worker + SharedArrayBuffer + TLS + IMAP greeting       PASS
cargo test -p mailbox-core --tests                              PASS
cargo test -p mailbox-ffi                                       8 passed
cargo test -p mailbox-cli --test serving                        32 passed
```

Stage 2 remains intentionally unimplemented here. The wasm Mailbox facade is
still path-backed and the L0 store is still in memory; browser configuration,
SQLite serialization, blobs, and OPFS persistence are the next storage seam.

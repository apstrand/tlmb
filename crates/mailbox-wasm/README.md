# mailbox-wasm hosted core

This crate contains Stages 0–2 of the hosted flavour requested by `hosted.md`,
not the complete hosted product. It gives the existing `mailbox_ffi::Mailbox`
facade a wasm-bindgen ABI and includes a browser page that exercises:

- the production SQLite schema, including an FTS5 query, in memory;
- a verified rustls client handshake whose TLS records cross a WebSocket relay;
- the real blocking IMAP connection running in a dedicated Worker over a
  SharedArrayBuffer `Read`/`Write` bridge;
- SQLite, compressed blobs, and sync bookkeeping restored from one atomic OPFS
  checkpoint; and
- account settings and credentials persisted in IndexedDB.

Build and open the page:

```sh
./crates/mailbox-wasm/build-web.sh
python crates/mailbox-wasm/serve.py
```

For the handshake probe, run the fixed-destination throwaway relay separately:

```sh
python crates/mailbox-wasm/spike-relay.py \
  --target imap.gmail.com:993
```

Then visit `http://127.0.0.1:8765/?run-tls=1`. `serve.py` sets COOP and COEP;
without those headers the browser correctly refuses to expose
SharedArrayBuffer. The relay forwards opaque binary frames and cannot decrypt
them. It is deliberately local-only by default and fixes its destination at
startup; it is not the hardened Stage 4 relay and must not be deployed as one.

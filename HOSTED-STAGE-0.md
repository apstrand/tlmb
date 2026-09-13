# Hosted flavour: Stage 0 report

Date: 2026-09-12

Verdict: **the proposed architecture is feasible.** None of the three Stage 0
questions blocked. Stages 1 and 2 still contain substantial engineering work,
but they no longer depend on an unproven SQLite or TLS capability.

## 1. SQLite and FTS5: pass

`rusqlite 0.40` selects its `sqlite-wasm-rs` backend for
`wasm32-unknown-unknown`; the bundled SQLite C source does not need to compile
for this target. In headless Chromium the probe:

- opened an in-memory `rusqlite::Connection`;
- ran the production schema migrations through schema version 7;
- created the production contentless FTS5 virtual table; and
- parsed and executed the FTS-backed query `hosted` successfully.

This required one small core seam, `Store::open_in_memory`. Persistence remains
out of scope: Stage 2 still needs SQLite serialization and OPFS storage.

## 2. TLS over WebSocket bytes: pass

The existing rustls/ring stack completed a certificate-verified TLS client
handshake with `imap.gmail.com:993` in headless Chromium. The browser exchanged
only opaque TLS records with the included fixed-destination WebSocket-to-TCP
spike relay.

No provider swap was needed. The target did need explicit feature selection:

- ring's `wasm32_unknown_unknown_js` feature for Web Crypto entropy;
- rustls-pki-types' `web` feature for browser wall-clock time; and
- Mozilla roots (`webpki-roots`) in place of `rustls-native-certs`, which does
  not support `wasm32-unknown-unknown`.

The test proves the cryptographic and byte-stream part of the design. Stage 1
must still build the blocking `Read`/`Write` transport on a Worker and
SharedArrayBuffer, including COOP/COEP hosting requirements. The Python relay
is a test instrument, not the policy-hardened Stage 4 component.

## 3. Remaining dependency graph: pass

The complete `mailbox-ffi`/`mailbox-core` graph builds for
`wasm32-unknown-unknown`. The browser smoke test also executed representative
runtime operations for:

- zstd compression and decompression;
- BLAKE3 hashing;
- `mail-parser` MIME parsing;
- `imap-proto` response parsing;
- Argon2 derivation;
- `time`; and
- JavaScript-backed `getrandom` at runtime (versions 0.2, 0.3, and 0.4 all
  compile in the graph).

The only host-specific compile failures encountered were expected: native
certificate discovery, `flock`, and the SIGKILL crash hook. They now have
target-specific selection or minimal wasm behavior. Native behavior is
unchanged.

## Artifacts and reproduction

The spike lives in `crates/mailbox-wasm`. It contains the wasm-bindgen wrapper,
the L0/runtime probe, the TLS record state machine, the throwaway page, and the
fixed-destination relay. See that directory's README for commands.

Verification performed:

```text
cargo build -p mailbox-wasm --target wasm32-unknown-unknown     PASS
Chromium: schema v7 + FTS5 + dependency runtime smoke           PASS
Chromium: verified TLS to imap.gmail.com:993 through relay      PASS
cargo test -p mailbox-core --lib                                13 passed
cargo test -p mailbox-ffi                                       8 passed
cargo test -p mailbox-cli --test serving                        32 passed
```

`cargo test --workspace` passed all tests until one existing Dovecot scenario,
`old_mail_is_fetched_lazily_and_sync_full_gets_it`, failed because the old
message body was already present. The same isolated test failed again. The
wasm changes do not touch body-prefetch selection or the L2 fixture, so this is
recorded as an apparently unrelated integration failure rather than hidden as
a green workspace run.

## Gate decision

Proceed to Stage 1. The likely hard work is the synchronous Worker transport,
not TLS itself. Before Stage 3, measure a release wasm bundle—the debug module
used by this spike is about 16 MiB, but it is neither optimized nor stripped
and is not a meaningful shipped-size number.

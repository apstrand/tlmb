# Hosted flavour: Stage 2 report

Date: 2026-09-13

Verdict: **the core now has a browser-capable storage seam.** Native mailbox
state keeps its existing on-disk layout, while the wasm worker can restore and
checkpoint the same state without touching a host filesystem.

## Implementation

- `ByteStorage` is the single interface for non-SQLite bytes. `FsStorage`
  maps logical keys to today's native files; `MemoryStorage` provides the
  synchronous worker-side implementation and a stable binary checkpoint.
- Compressed MIME blobs and `state.toml` now use the store's `ByteStorage`.
  Config, policy, listing history, the OAuth access-token cache, and trace
  output expose the same storage-backed paths while their native entry points
  remain unchanged.
- SQLite runs in memory on wasm and uses rusqlite's serialization API. The
  wasm checkpoint combines the SQLite image and byte-store image in one OPFS
  file, so a browser observes the old complete store or the new complete store.
- Account settings and credential records live in separate IndexedDB object
  stores. Browser diagnostics never render the credential value.

## Browser verification

The Stage 2 page was loaded twice with the same Chromium profile. On the first
load it atomically wrote and reopened a checkpoint containing schema v7, an
idempotent folder, a zstd-compressed MIME blob, and sync state. On the second
load it reported:

```json
{
  "opfs": {
    "ok": true,
    "restored": true,
    "before": 1,
    "after": 1,
    "reopenedCount": 1,
    "byteStorageRestored": true
  },
  "settings": {
    "ok": true,
    "restored": true,
    "account": "browser"
  }
}
```

The same run remained cross-origin isolated and the Stage 1 worker completed
the real rustls + Gmail IMAP capability exchange.

## Verification

```text
cargo build -p mailbox-wasm --target wasm32-unknown-unknown     PASS
cargo test -p mailbox-core --test storage_seam                  2 passed
cargo test -p mailbox-core --test store                         34 passed
Chromium reload: SQLite + blobs/state from OPFS                 PASS
Chromium reload: settings + credential record from IndexedDB    PASS
Chromium Worker + SharedArrayBuffer + Gmail TLS/IMAP             PASS
```

Stage 3 is next: generate the static site, select the wasm backend at the
client API choke point, and expose hosted account setup and message bodies.

# TODO

Deferred work, with enough context to pick it up cold. Milestone work lives in
[SPEC.md](SPEC.md) §15; this is the finetuning that has been consciously put
off, and the gaps found along the way.

## Behaviour to finish

- **Mark a message read when it is opened.** Only `N` in the TUI and
  `mailbox flag <id> +seen` do it today. §7 asks for mutt-like muscle memory and
  mutt marks a message seen on open, so this is an omission rather than a
  decision. Probably wants a short dwell so cursoring past a message in the
  index does not mark it.
- **Background sync.** There is no daemon, no timer, and no `IDLE`: `sync --once`
  is still the only mode, so queued writes reach the server when the user runs
  `mailbox sync` or presses `@`. §5.3 wants `IDLE` per selected folder plus a
  bounded pool of idling folders, with a poll fallback and backoff.
- **§4.1's owner lock and socket.** Designed, not built. Reads already take the
  read-only path, which is the half that matters most; what is missing is the
  advisory lock that makes exactly one process the sync engine, and the Unix
  socket that lets the losers have their writes performed.

## Verification gaps

- **L2 against Cyrus and Stalwart.** M2 requires it (§15) and it is the one
  milestone item still open. Needs Docker, which this machine's user no longer
  has access to. Stalwart is the valuable one: it speaks IMAP *and* SMTP
  submission, so it would give a genuine end-to-end send round trip.
- **L2 against Dovecot is currently skipping** for the same reason, so those 38
  tests pass vacuously. `MAILBOX_L2_REQUIRED=1` turns that into a failure.
- **Re-record the L1 corpus from a real server.** The committed transcripts came
  from the in-tree L3 server; §14.2 wants them from L2 and L4 runs.
  `cargo test -p mailbox-cli --test record_corpus -- --ignored` does it.
- **The §14.5 perf budgets want a dedicated runner.** CI gates on query plans,
  which are machine-independent; the timings are opt-in behind
  `MAILBOX_PERF_MESSAGES=1000000 MAILBOX_PERF_BUDGETS=1`.

## Known coarseness

- **Attachment fetching is per message, not per part.** §5.1 wants attachments
  fetched on demand unless small (< 256 KB); today a message is fetched whole or
  not at all, decided by the `offline` policy and the prefetch window. Doing it
  properly needs `BODYSTRUCTURE`-driven partial fetch and a way to hold a
  message whose large parts are still on the server.
- **`raw:` search passthrough is parsed and ignored.** It compiles to "match
  everything" locally; the sync engine does not yet hand it to the server.
- **`COMPRESS=DEFLATE`** is not used, though it is advertised by most servers.

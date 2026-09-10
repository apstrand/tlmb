# TODO

Deferred work, with enough context to pick it up cold. Milestone work lives in
[SPEC.md](SPEC.md) §15; this is the finetuning that has been consciously put
off, and the gaps found along the way.

## Behaviour to finish

- **A pool of idled folders.** `sync --watch` idles on the inbox, which is
  where the mail someone is waiting for arrives. §5.3 wants "a bounded pool of
  idling folders (default: inbox and any folder viewed in the last 24 h)",
  which needs a connection per folder and a thread to hold each. Worth doing
  after the single case has been run against real servers.
- **Idling for more than one account.** `IDLE` selects one mailbox on one
  connection, so a workspace with several accounts polls them all. `--account`
  narrows back to the idling path. A thread per account is the fix.
- **§4.1's owner socket.** The advisory lock is built: exactly one process is
  the sync engine, and a loser is told so — truthfully, since its local writes
  are already queued and the owner pushes them. What is missing is the socket
  that would let the loser have its writes performed *now*, and share the warm
  connection pool.

- **`watch` syncs for all its accounts or none.** It takes ownership (§4.1) of
  every account in scope or gives up on syncing entirely, because the sync loop
  takes a whole workspace and partial ownership would mean quietly following
  some accounts while syncing others. `--account` narrows it back to a single
  one, which is the honest workaround. A thread per account fixes this and the
  two idling items above together.

- **Discovery has no end-to-end test.** The chain in `discover.rs` is covered
  per source — the two XML formats, the DNS wire format, the ordering — over
  recorded documents, and its network half has been run by hand against a real
  domain. What is missing is a test of the whole thing, which needs an HTTPS
  server the suite does not have: `Target::parse` allows plaintext only to
  loopback, so an L3 autoconfig server would need a certificate and a trust
  root the client accepts. Worth doing when the first discovery bug arrives
  from a real domain; the shape of it would be an L3 HTTPS server with a
  generated root, which the OAuth2 L3 server would want too.
- **No OAuth2 screen in the iOS shell.** The flow is in the core and the FFI
  can add an account with `oauth_provider`, but nothing there drives
  `oauth::login` — and a loopback redirect is not how a phone does this: iOS
  wants `ASWebAuthenticationSession` and a custom URL scheme. The core's
  `Redirect` would need a sibling that takes the callback URL from the shell.

## M8, in progress

The §5.4 state folder is the entry point: `mailbox devices` and revocation
(§8.3) ride on it, and the short code and relay cannot ship at all until the
PAKE has had the outside review §16 requires.

What is built is the substrate — the record model, the fold, and the wire
format (`state/`), plus the key it is sealed with and the pairing that carries
it. What is not:

- **The IMAP transport.** Nothing reads or writes `Mailbox/.state` yet. It
  wants an `APPEND` per new record, a fetch of what the folder gained since the
  last pass, and a place in the sync loop — the folder is a folder, so the
  existing per-folder UID bookkeeping mostly applies, but it must be excluded
  from `ls`, from search, and from the unified inbox.
- **Announcing the folder on first sync.** §5.4 is emphatic that creating a
  folder in someone's account is an imposition, so it is announced, the name is
  configurable, and `state_sync = false` is honoured permanently. The config
  field exists and nothing reads it yet.
- **Nothing is wired to the store.** The fold produces a `View`; the tags,
  snooze, and saved-search tables are still the local-only ones, and reconciling
  them in both directions (a local tag becomes a record; a record becomes a
  row) is the next piece after the transport.
- **Compaction.** The log only grows. §5.4 wants devices to compact the folder
  periodically, which means writing a snapshot record and expunging what it
  subsumes — and doing that safely while another device is appending.
- **`mailbox devices` and revocation.** The `Device`/`RevokeDevice` entries and
  the revocation-is-permanent rule are in the fold; the command, the key
  rotation, and the printed list of credential rotations §8.3 asks for are not.

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

- **Discovery's SMTP host is a guess even when its IMAP host was verified.**
  The probe step connects to confirm IMAP and then picks `smtp.<domain>` or the
  same host without connecting — a submission server greets anyone, so the
  probe would prove only that something listens, and the first send reports
  honestly (§6.6). A real check would be `EHLO` plus an `AUTH` mechanism the
  account can actually use, which is worth doing once there is a second server
  to try it against.

- **Attachment fetching is per message, not per part.** §5.1 wants attachments
  fetched on demand unless small (< 256 KB); today a message is fetched whole or
  not at all, decided by the `offline` policy and the prefetch window. Doing it
  properly needs `BODYSTRUCTURE`-driven partial fetch and a way to hold a
  message whose large parts are still on the server.
- **`raw:` search passthrough is parsed and ignored.** It compiles to "match
  everything" locally; the sync engine does not yet hand it to the server.
- **`COMPRESS=DEFLATE`** is not used, though it is advertised by most servers.

## After M5

- **Share the Markdown parser.** The TUI styles bodies as Markdown; the GUI and
  the web interface render the sanitized HTML directly. The inline rules — what
  counts as emphasis, when a marker is punctuation — live in
  `mailbox-tui/src/markdown.rs`, and a second copy would be two sets of subtle
  rules that drift. Extracting the parser into a crate the TUI and any other
  text-mode view can use is the fix; duplicating it is not.
- **§15's "the wasm build runs L0 in-browser" no longer has a wasm build.**
  The web interface is DOM now (§16), so there is nothing compiled to wasm to
  run L0 in. The clause needs rewriting or dropping: what it was buying — proof
  that the parsers behave the same off the host — is worth keeping, and M6's
  mobile targets are where it lands.
- **The web interface has no automated browser test on the host.** Its API is
  covered by `serving.rs`; the DOM client is driven in the KVM guest of
  `tests/e2e/` (`mailbox-e2e run web`). A headless run *on this machine* is
  still missing if you want that without a VM.
- **The narrow layout is unverified visually.** The rules and the pane
  switching are exercised. `mailbox-e2e` shots are 1280×720; a phone-shaped
  resize in the guest is still to do.
- **Attachments in compose.** §7 asks for "attachment drag/paste/`--attach`";
  the CLI has `--attach` and neither window does. It wants a multipart upload
  route and a size the browser will not choke on.
- **Drafts are not saved.** §7 wants them "stored in the outbox and synced to
  the server's Drafts folder". Closing a compose window discards it — the
  window warns nobody, and it should.
- **No display name on an identity.** The *From* selector shows bare
  addresses, because no config field holds a name. Mail goes out without one.
- **Markdown→HTML compose.** §7 offers it optionally; both windows send plain
  text only.

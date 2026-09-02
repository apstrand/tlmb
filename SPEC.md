# mailbox

A self-contained IMAP & SMTP client. One Rust core, many faces: CLI, TUI, GUI,
web, and native mobile. Fast, lean, and — above all — it gets out of the way.

## 1. Goals

1. **Fast.** Every interaction is instant on a mailbox with a million messages.
   Startup, search, and navigation are budgeted (§10), not aspirational.
2. **Out of the way.** Minimal configuration, no account setup wizard beyond an
   address and a password/OAuth consent, no mandatory cloud, no telemetry, no
   nagging. Sane defaults, few knobs, all of them documented in one page.
3. **Dual-audience CLI.** The same commands serve a human at a prompt and an
   agent driving the binary programmatically. Neither is a second-class citizen
   (§6).
4. **Self-contained.** A single static binary per platform carries the sync
   engine, the local store, all four interfaces, and the web server. No daemon
   to install, no runtime, no database server, no external indexer.
5. **Multi-account, multi-device, multi-instance.** Several accounts feel like
   one mailbox. Several running instances (phone, laptop, a shell) converge
   without stepping on each other.
6. **Portable identity.** Moving to a new host or device is a sub-minute
   operation: scan a QR code, pair over the LAN, or — from M8 — type a short
   code (§8). No re-entering passwords, no re-doing OAuth, no config file
   archaeology.

## 2. Non-goals

- Not a mail server, MDA, or MTA relay. It speaks IMAP and SMTP submission and
  nothing else on the wire.
- No calendar, no contacts sync, no task manager, no chat. Address completion is
  derived from mail traffic, not from a CardDAV client.
- No plugin runtime, no embedded scripting language, no extension marketplace.
  Extension happens through the CLI and its JSON contract.
- No proprietary backend. The vendor-operated pieces (a stateless pairing relay,
  optionally a push notifier) are optional, self-hostable, and store nothing.
- No telemetry, analytics, crash-phone-home, or network access the user did not
  initiate.

## 3. Platforms

| Platform | Interfaces |
|---|---|
| Linux, macOS, BSD | CLI, TUI, GUI, web |
| iOS, Android | native GUI, web (local) |
| Anywhere | web, served by any of the above |

The web interface is compiled into every build. `mailbox serve` (alias
`mailbox --serve`) starts it; by default it binds `127.0.0.1` on an ephemeral
port, prints a URL with a one-time token, and prints a QR code for that URL so a
phone on the same LAN can open it (`--bind 0.0.0.0` required for off-host
access, and it refuses to bind non-loopback without a token). It is the *same*
UI the desktop GUI renders — one implementation, two shells.

Mobile builds are the Rust core compiled to a static library with a thin native
UI layer (SwiftUI / Jetpack Compose). No web view for the main UI.

## 4. Architecture

```
                +---------------------------------------------+
   frontends    |  CLI    TUI    GUI    web (wasm)   mobile UI |
                +-----------------------+---------------------+
                                        | mailbox-api (in-proc or unix socket / http+json)
                +-----------------------+---------------------+
   core         |  session: query, compose, flags, threading   |
                |  store: sqlite (meta+fts5) + blob cas        |
                |  sync: imap engine, smtp submit, outbox      |
                |  auth: keychain, oauth2, credentials         |
                +---------------------------------------------+
```

- **`mailbox-core`** — store, sync engine, query, threading, MIME. No I/O policy,
  no UI.
- **`mailbox-api`** — one request/response contract, serialized as JSON. Used
  in-process by the TUI/GUI, over a Unix socket by additional CLI instances, and
  over HTTP by the web UI. The CLI's `--json` output *is* this contract, so
  anything the GUI can do, a script can do.
- **Frontends** are thin. If a frontend needs logic that isn't in the core, that
  logic belongs in the core.

### 4.1 Instance coordination

Three constraints decide this design:

1. **Servers cap concurrent IMAP connections.** Gmail allows roughly 15 per
   account; plenty of Dovecot and Cyrus deployments are configured at 3–10.
   Each idling folder holds one, so a TUI already consumes several. If every
   process opened its own connections, a handful of concurrent CLI invocations
   would hit the cap — and servers answer that with throttling or a temporary
   account lock, which is a far worse failure than a slow command.
2. **A cold connection costs 200–600 ms** (TLS handshake, AUTH, SELECT) before
   the first useful byte. §10 budgets a warm CLI command at 20 ms. Per-process
   connections miss that by an order of magnitude and no local cache repairs it
   for a command that must touch the server.
3. **Applying a sync delta is a transaction wider than SQLite's locking.** It
   spans `meta.db`, the blob CAS, and the UIDVALIDITY/HIGHESTMODSEQ bookkeeping.
   SQLite would serialize the individual writes correctly and still let two sync
   engines interleave into a coherent database full of incoherent mail:
   duplicated messages, a MODSEQ asserting deltas that were never applied, an
   outbox entry sent twice. Exactly one process per account may be the sync
   engine.

So: there is no separately installed daemon, and the first process to touch an
account acquires an advisory lock to become the **owner** — the sole holder of
the connection pool, the sole runner of IDLE, and the sole writer to the store.
The lock decides who may be the sync engine; the Unix socket
(`$XDG_RUNTIME_DIR/mailbox/<account>.sock`) exists because the processes that
lose still need their writes performed and still need the warm connection pool.
A CLI invocation while the TUI is open is therefore a socket round-trip, not a
second sync engine. If the owner exits, the lock releases and the next process
takes over.

Reads do not participate at all: a process that only needs to read opens the
store read-only (SQLite WAL) and skips ownership entirely, which is the fast
path for scripts and agents.

The socket is a deliberate choice over the simpler alternative of having
non-owners append to a queue table for the owner to drain. Queueing loses the
synchronous result — `mailbox mv` could no longer answer with `conflict` (§6.6)
— and callers that depend on structured, immediate errors, agents above all,
are the ones that would pay for it.

Across devices there is no coordination protocol: the IMAP account is the source
of truth (§5.3).

## 5. Data & sync

### 5.1 Local store

Per account, under `$XDG_DATA_HOME/mailbox/<account-id>/`:

- `meta.db` — SQLite (WAL): messages, folders, flags, threads, address book,
  sync state. FTS5 index over headers and extracted text. **FTS5 is the search
  backend on every platform** — one file, one fsync domain, no second index to
  keep coherent with the store, and it runs unchanged on mobile. A separate
  engine (Tantivy) is explicitly not planned; revisit only if the §10 full-text
  budget is missed at 1M messages after tuning.
- `blobs/` — content-addressed, zstd-compressed raw MIME (`blobs/ab/cdef…`).
  Deduplicated across folders, so a message in `INBOX` and `[Gmail]/All Mail` is
  stored once.
- `state.toml` — non-secret sync bookkeeping (UIDVALIDITY, HIGHESTMODSEQ).

Bodies are fetched lazily by default: headers and structure for everything,
bodies for what you read plus a configurable prefetch window (default: 30 days
of the unified inbox, and every thread you open). `mailbox sync --full` fetches
everything for offline use. Attachments are fetched on demand unless small
(< 256 KB) or the account is marked `offline = "full"`.

### 5.2 Identity of a message

CLI and API expose a stable `id`: a BLAKE3 of the account ID plus the
`Message-ID` header, or, absent a usable `Message-ID`, of
(Date, From, Subject, RFC822.SIZE). It survives folder moves, UIDVALIDITY
resets, and re-downloads, and it is the same on every device. Commands accept
any unambiguous prefix (7 hex chars shown by default), plus these conveniences:

- `@` — the message currently selected in the TUI/GUI, if any
- `^` — the most recent message printed by the previous CLI command
- `1..20` — index into the last listing (session-scoped, humans only; agents
  should use IDs and are warned when they don't)

### 5.3 Server sync

- IMAP4rev1/rev2 with `CONDSTORE` + `QRESYNC` where offered, so reconnect is a
  delta, not a rescan. Fallback to UID-window diffing.
- `IDLE` per selected folder plus a bounded pool of idling folders (default:
  inbox and any folder viewed in the last 24 h); poll fallback with backoff.
- `MOVE`, `UIDPLUS`, `LIST-EXTENDED`, `SPECIAL-USE`, `COMPRESS=DEFLATE`,
  `BINARY`, `SORT` used when advertised, never required. Server-side `THREAD`
  is deliberately not used (§5.6).
- Gmail: `X-GM-EXT-1` for labels and `X-GM-RAW` search passthrough; All Mail
  deduplicated against label folders.
- Writes are optimistic: local state changes immediately, the operation is
  queued, and the queue is replayed with idempotent semantics on reconnect.
  Conflicts resolve to the server's flags with a note in `mailbox status`.

### 5.4 Cross-device state that IMAP can't carry

Tags, saved searches, per-account UI preferences, snooze times, and the address
book need to follow the user without a third-party server. They are stored as
append-only, encrypted records in a dedicated IMAP folder (default
`Mailbox/.state`, hidden from the UI). Each record is a small MIME message;
devices compact the folder periodically. The encryption key is part of the
pairing payload (§8), so a paired device can read the state and a server
operator cannot.

**Decision:** on by default (`state_sync = true`) wherever the server permits
folder creation. Creating a folder in someone's account is a real imposition, so
it is announced on first sync, the folder name is configurable, and
`state_sync = false` is honored permanently.

**This ships late — M8, not with pairing.** Until then the state is local, and
pairing carries a point-in-time snapshot instead of a live feed. Everything that
depends on the folder inherits that schedule: shared snooze times, and the
device list and revocation of §8.3.

### 5.5 Sending

Outgoing mail goes to a durable local **outbox** first, then to SMTP submission.
Sends are retried with backoff, survive restarts, and are deduplicated by an
idempotency key (§6.5). `Sent` upload uses `APPEND` unless the server files sent
mail itself (Gmail). Undo-send window is configurable (default 5 s interactive,
0 for the CLI unless `--delay` is given).

### 5.6 Threading

Threads are computed locally with the JWZ algorithm over `References` and
`In-Reply-To`, always, on every device and every platform. Server-side `THREAD`
is not used even where advertised: implementations disagree about the same
mailbox, so relying on it would mean the same conversation is grouped one way on
a laptop and another way on a phone. Local threading is deterministic, works
offline, and costs a fraction of the §10 budget.

JWZ's subject-based grouping of otherwise unlinked messages is enabled but
narrowly scoped (same normalized subject, within 14 days); `threading.subject =
false` disables it for users whose mail defeats the heuristic.

## 6. CLI

The CLI is the reference interface. The TUI and GUI are conveniences over it.

### 6.1 Shape

```
mailbox [global flags] <command> [args]

  ls [query]              list messages (default: unified inbox, unread first)
  show <id>               render a message or thread
  search <query>          full-text + structured search
  compose                 open $EDITOR, then send or queue
  send                    non-interactive send (flags or RFC5322/JSON on stdin)
  reply <id>              reply / reply-all / forward (--all, --forward)
  flag <id>… <flags>      +seen -flagged +tag:invoices …
  mv <id>… <folder>       move; cp, rm, archive, spam are sugar over it
  attach <id> [part]      list or extract attachments
  folders                 list folders with counts
  accounts                add / list / remove / test accounts
  sync                    force a sync pass; --full, --folder, --since
  watch                   stream events as NDJSON until interrupted
  pair                    device pairing / migration (§8)
  serve                   start the web UI
  tui | gui               launch an interface
  mcp                     serve the MCP tool interface on stdio
  status                  queues, errors, connectivity, store size
  schema [command]        emit JSON Schema for a command's output
  debug <subcommand>      introspection hooks (§6.7)
```

Global flags: `--json`, `--ndjson`, `--fields a,b,c`, `--account`, `--limit`,
`--cursor`, `--dry-run`, `--yes`, `--quiet`, `--color=auto|always|never`,
`--timeout`, `--config`, `--profile`, `--trace[=path]`.

### 6.2 Query language

One grammar everywhere — CLI, TUI filter bar, GUI search box, API:

```
from:alice to:me subject:"quarterly report"
is:unread is:flagged has:attachment
in:inbox in:archive tag:invoices thread:<id>
before:2026-08-01 after:-7d larger:5M filename:*.pdf
alice AND (invoice OR receipt) NOT is:spam
```

Bare words are full-text. `-7d`, `yesterday`, `last-week` are accepted where a
date is. Unknown keys are an error, not a silent full-text fallback — agents
must not get plausible-looking wrong results. `raw:` passes a native IMAP
`SEARCH` through when the server supports it.

### 6.3 Human affordances

Colored, aligned, terminal-width-aware output; a pager only when stdout is a TTY
and the output exceeds a screen; `NO_COLOR` honored; unicode/ASCII table styles
autodetected; sensible short aliases; shell completions for bash/zsh/fish/nu
generated by `mailbox completions`.

### 6.4 Agent affordances

Every one of these is a hard requirement, not a nicety:

- **`--json` on every command**, emitting exactly one JSON document; `--ndjson`
  for streaming lists and `watch`. Schemas are stable, versioned
  (`"schema": "mailbox/v1"`), and dumpable via `mailbox schema <command>`.
- **Errors are structured**: on `--json`, failures print
  `{"error":{"code":"auth_failed","message":…,"retryable":false,"hint":…}}` to
  stderr and set a distinct exit code (§6.6). Never a bare English sentence.
- **No interactive prompts when stdin is not a TTY.** A command that would have
  prompted fails with `needs_confirmation` and tells the caller which flag
  supplies the answer.
- **Deterministic output**: explicit sort order, stable field order, cursors
  (`--cursor`) instead of offsets, no ANSI, no spinners, no progress bars.
- **Token-aware rendering** for LLM consumers: `show --format=md` converts HTML
  mail to Markdown; `--text-only`, `--strip-quotes` (drops quoted replies and
  signatures), `--max-body-bytes N` (truncates with an explicit
  `"truncated": true`), `--no-attachments`.
- **`mailbox watch --json`** streams `{event: "new"|"flag"|"move"|"sent"|…}` so
  an agent can run a loop without polling.
- **`mailbox mcp`** exposes the same operations as MCP tools over stdio, with
  the read/write split and policy of §6.5 enforced identically.
- **Read-only by default in agent mode**: `MAILBOX_AGENT=1` (or `--profile
  agent`) makes any state-changing command require an explicit `--yes`.

### 6.5 Sending safely from automation

Agents that can send mail unsupervised are a liability. The spec bakes in the
guardrails rather than leaving them to the caller:

- **Idempotency**: `mailbox send --idempotency-key <k>` records the key with the
  outbox entry; a retry with the same key returns the original result instead of
  sending twice. Keys expire after 7 days.
- **Approval queue**: with `send_policy = "approve"` (default when
  `MAILBOX_AGENT=1`), `send` enqueues and returns
  `{"status":"pending_approval","id":…}`. The message sits in the outbox until a
  human releases it from the TUI/GUI or via `mailbox outbox approve <id>`.
- **Policy file** (`policy.toml`): allowed/denied recipient patterns, max sends
  per hour, maximum recipients per message, external-domain confirmation,
  attachment size caps. Violations fail with `policy_denied` and cite the rule.
- `--dry-run` renders the exact bytes that would be transmitted and exits 0
  without touching the network.

### 6.6 Exit codes

`0` success · `1` unspecified failure · `2` usage error · `3` not found ·
`4` authentication/authorization · `5` network/offline · `6` conflict (needs
resync) · `7` policy denied · `8` needs confirmation · `9` timeout.

### 6.7 Determinism & introspection hooks

These exist because §14 cannot be written without them. They are shipped, not
`#[cfg(test)]`, because the same hooks are what makes a bug report actionable —
but they are inert unless asked for, and `debug` is hidden from `--help`.

- **`--trace[=path]`** — NDJSON of every IMAP command and response (credentials
  and message bodies redacted unless `--trace-bodies`), every store transaction,
  and every state machine transition, each with a monotonic sequence number.
  Tests assert on the event sequence instead of on wall-clock behavior, and a
  user reporting a sync bug can attach one.
- **`sync --once`** — run exactly one sync pass to quiescence and exit; no
  background loop, no IDLE. Every end-to-end test steps the engine with this.
  `--until <event>` stops at a named trace event for mid-operation assertions.
- **`debug store-dump [query] --json`** — canonical, ordered dump of derived
  state (threads, flags, counts, outbox) for golden-file comparison. Field order
  and sort are stable by definition; this is the convergence oracle of §14.
- **`debug clock`** and `MAILBOX_CLOCK` / `MAILBOX_SEED` — pin the clock and the
  RNG. Idempotency keys, pairing secrets, snooze deadlines, and message IDs are
  all derived from one injected source each, never from ambient `now()` or
  thread-local randomness. Any code path that reaches for the ambient version is
  a bug the lint catches.
- **`MAILBOX_CRASH_AT=<event>[:n]`** — kill the process at a named trace event,
  the way a power cut does: no destructors, no `atexit`, nothing flushed. This
  is what makes the crash-only oracle (§14.3) a supervisor loop rather than a
  hope, and it works with or without `--trace` — a hook that silently did
  nothing unless tracing was on is how a crash suite goes green while testing
  nothing.
- **`debug fixture load <file>`** — seed a store or an account from a fixture
  manifest (§14.1).
- **`MAILBOX_STATE_DIR` / `MAILBOX_CONFIG`** — relocate every path the process
  touches, so tests run in parallel with no shared state and no `$HOME`
  pollution.

## 7. Interfaces

**TUI** — mutt-like muscle memory: `j/k`, `/` search, `t` tag, `d` delete, `s`
save, `m` compose, `r`/`R` reply, `|` pipe to a command. Vim-ish by default,
with an Emacs keymap and a fully remappable `keys.toml`. Everything is
incremental: the index list paints before sync completes; searching filters as
you type; no modal "please wait" states, ever.

**GUI** — Spark-like: unified inbox, thread cards, a single search field that
takes the §6.2 grammar, swipe/keyboard triage (archive, snooze, tag — see the
snooze note below), and a
compose window whose *From* selector is one keystroke away and defaults to the
identity the thread was addressed to. Multi-account is invisible until you want
it: one list, per-message account color, `account:` to split.

**Web** — the GUI compiled to wasm, served locally, same keybindings, works
offline via the local store on the serving host.

**Mobile** — the same model with a touch-first layout. Background refresh within
each OS's limits; optional push via the notifier of §11.

**Snooze** is a low-priority feature and starts local-only: a timer plus a local
state record, which means a snoozed message reappears on the device that snoozed
it, whenever that device is next running. This is an honest limitation, so the
UI says which device holds the snooze rather than implying it is synced. Shared
snooze across devices arrives with the §5.4 state folder in M8.

**Compose** (all interfaces): identity selection from the account list plus any
alias, remembered per correspondent and per thread; plain text by default with
optional Markdown→HTML multipart; attachment drag/paste/`--attach`; drafts
stored in the outbox and synced to the server's Drafts folder.

## 8. Account migration & device pairing

Adding a second device, or moving to a new host, must not mean re-entering
credentials or redoing OAuth. `mailbox pair` transfers a **payload** —
account definitions, credentials and OAuth refresh tokens, identities/aliases,
the §5.4 state key, and UI preferences — from an enrolled device to a new one.
It never transfers the mail store; the new device syncs that from IMAP.

Three transports, in order of preference:

1. **QR (offline)** — the receiving device shows nothing; the sending device
   renders the payload directly as one or more QR codes (animated/chunked when
   large) and the receiver's camera reads them. No network, no relay. Available
   where the receiver has a camera.
2. **LAN (offline)** — mDNS discovery on the local network, direct TLS
   connection between the two devices, authenticated by the pairing secret.
3. **Relay (anywhere)** — a stateless rendezvous service for the
   camera-less/different-network case, e.g. a laptop being set up from a phone
   in another city. **Deferred to M8**: v1 ships transports 1 and 2 plus the
   encrypted export file, and the relay lands once the PAKE implementation has
   had an outside review. Shipping a half-reviewed PAKE is how you turn a
   45-bit code into a credential leak, and QR + LAN cover the common cases.

### 8.1 Cryptography

The relay must never be able to read a payload, and must never hold anything
worth stealing.

```
sender                          relay (stateless)                receiver
  |  pairing secret S ---------- out of band (QR or spoken code) ---> |
  |                                                                  |
  |-- POST /r/{cid}  ciphertext ---->  in-RAM, TTL 300s, one GET ---->|
  |                                                                  |
  both display SAS = HKDF(S,"sas") -> 4 words / 6 digits; user compares
```

- **QR / long code path**: `S` is 256 bits of CSPRNG output, carried entirely in
  the QR (or a `mailbox://pair#…` link). `cid = HKDF(S,"channel")`,
  `k = HKDF(S,"key")`; payload is sealed with XChaCha20-Poly1305 under `k` with
  the payload's schema version as AAD. The relay sees an opaque channel ID and
  opaque bytes.
- **Short code path** (M8, with the relay): a 9-character human-typable code (Crockford base32, no
  ambiguous glyphs, ~45 bits) is too weak to be a key, so it is used as the
  password in a balanced PAKE (CPace, X25519 group). The relay carries the two
  PAKE messages and then the sealed payload; an attacker gets one online guess
  per channel and learns nothing offline. The relay rate-limits and burns the
  channel after a single failed handshake.
- **Confirmation**: both devices display a short authentication string derived
  from the transcript. The user confirms they match before the payload is
  accepted. This is what defends the LAN and relay paths against an active MITM.
- **Freshness**: channels live at most 300 s and are consumed by the first
  successful fetch. The payload carries a timestamp and a nonce; a receiver
  rejects a payload older than the TTL.

### 8.2 The relay service (M8)

Deliberately trivial, so that running your own is a five-minute job and trusting
the default one is a small decision:

```
POST /r/{cid}      body: opaque bytes ≤ 64 KB   -> 201, or 409 if occupied
GET  /r/{cid}                                   -> 200 + bytes, then deleted
                                                   404 if absent/expired
```

- In-memory only. No database, no disk, no logs beyond aggregate counters. No
  credentials, plaintext, account addresses, or IPs are persisted.
- TTL 300 s, single-fetch, 64 KB cap, per-IP and per-channel rate limits.
- Anonymous: no accounts, no API keys, nothing linking two channels.
- Endpoint is configurable (`pair.relay = "https://…"`); a single binary
  (`mailbox relay`) runs it, so self-hosting is `mailbox relay --bind :8443`.
- If the relay is unreachable — or has not shipped yet — `pair` falls back to
  printing the QR and telling the user to use transport 1 or 2.

### 8.3 Flow and revocation

The short-code flow below is the M8 shape; through M4–M7 the same command shows
only the QR and the LAN offer.

```
new device:   $ mailbox pair
              → Enter this on your other device:   K7M-4QB-XZ2
              → or scan: ▓▓▒▒…

old device:   $ mailbox pair send K7M-4QB-XZ2      (or scans the QR)
              → 3 accounts, 2 identities → confirm words: amber-tiger-04-mint
new device:   → confirm words: amber-tiger-04-mint  [y/N]
              → paired. syncing 3 accounts…
```

- Pairing is **additive** by default (`--merge`); `--replace` wipes the target's
  existing accounts, and requires `--yes`.
- `mailbox pair --export-file backup.age` writes the same payload sealed with a
  passphrase for air-gapped moves; `mailbox pair --import-file` reads it.
- `mailbox devices` (M8, since it rides on the §5.4 state folder) lists paired
  devices (name, platform, last seen), and `mailbox devices revoke <name>` rotates the state
  key and marks the device's records dead. Note that revocation cannot claw back
  IMAP credentials already copied: the command therefore prints the exact
  password/token rotations the user should perform, and offers to walk through
  them.

## 9. Accounts, auth, security

- **Setup is one line**: `mailbox accounts add peter@example.com`. Server
  settings are discovered via `autoconfig`/`autodiscover`, DNS SRV, the Mozilla
  ISPDB, and a small built-in table for the big providers. Manual overrides
  exist but are never required for a compliant host.
- **OAuth2** (XOAUTH2/`OAUTHBEARER`) for Google/Microsoft/Yahoo, via the system
  browser with PKCE and a loopback redirect. Refresh tokens are stored like any
  other credential.
- **Credential storage**: OS keychain first (Secret Service, macOS Keychain,
  iOS/Android Keystore). Where none exists, an age-encrypted `credentials.age`
  unlocked by a passphrase, an SSH key, or a `pass`/`gpg`/command hook
  (`credential_command`). Credentials never land in the config file or in
  `meta.db`.
- **Transport**: TLS 1.2+ with system roots, required by default; STARTTLS only
  with `require_tls = true` enforcement; certificate pinning available per
  account; plaintext refused unless the host is loopback and the user opted in
  explicitly.
- **Message rendering**: no remote content, no cookies, no JavaScript, no
  automatic external font/image loads. "Load remote content" is per-sender and
  remembered. HTML is sanitized to a small safe subset before rendering.
  Attachments are never auto-opened; `attach --extract` writes with the
  executable bit cleared and mangles dangerous extensions.
- **At rest**: the store is unencrypted by default (it is your mail, on your
  disk, under your user's permissions, mode 0700), with an opt-in encrypted
  store for laptops (`store_encryption = true`, key in the keychain).
- **PGP/S-MIME**: verification and decryption via a delegated command
  (`gpg`/`rnp`) — out of scope for v1's own crypto, wired in behind a flag so
  signatures at least render honestly rather than as noise.

## 10. Performance budgets

Measured on a reference laptop (2020-class x86-64 laptop, NVMe) against a
1,000,000-message store, and enforced by benchmarks in CI:

| Operation | Budget |
|---|---|
| `mailbox ls` cold process start → first row | < 100 ms |
| Warm CLI command through the owner socket | < 20 ms |
| Structured search (`is:unread from:…`) | < 30 ms |
| Full-text search, first page | < 100 ms |
| TUI keystroke → repaint | < 16 ms |
| Open a cached thread | < 50 ms |
| Idle RSS, one account, TUI open | < 80 MB |
| Idle CPU while IDLE-connected | ~0% |
| Binary size, stripped, one platform | < 25 MB |

Rules that follow from these: no blocking I/O on a UI thread; every list is
paginated and virtualized; sync never blocks a read; the store is opened lazily
and read-only where possible; startup does zero network I/O before painting.

## 11. Notifications

Desktop notifications via the platform's native mechanism, filtered by a query
(default `is:unread in:inbox NOT tag:muted`). Mobile push is the one place a
server is unavoidable: v1 ships **no push by default** (background IMAP fetch
within OS limits), with an optional self-hostable notifier that holds an IMAP
IDLE connection and sends a contentless "check now" wake-up through APNs/FCM.
The notifier is separate from the pairing relay, opt-in, and documented as the
one component that needs the account's credentials if the user chooses to use
the hosted flavor.

## 12. Configuration

`$XDG_CONFIG_HOME/mailbox/config.toml`, everything optional:

```toml
[general]
editor          = "$EDITOR"
unified_inbox   = true
send_policy     = "immediate"        # immediate | approve
state_sync      = true                # §5.4, effective from M8

[accounts.work]
address    = "peter@example.com"
identities = ["peter@example.com", "p.strand@example.com"]
offline    = "recent"                # none | recent | full

[pair]
relay = "https://pair.mailbox.example"

[keys]                                # optional keymap overrides
archive = "e"
```

`mailbox config get|set|edit|explain <key>` manages it; `explain` prints what a
key does, its default, and where the current value came from. Config changes are
picked up live by running instances.

## 13. Interoperability

- Import from Maildir, mbox, Thunderbird, and Apple Mail exports:
  `mailbox import <path>` (uploads to IMAP or stores locally).
- Export: `mailbox export --format=maildir|mbox <query> <dest>`.
- `mailbox show --raw` emits exact RFC5322 bytes; `mailbox send` accepts them,
  so `mailbox | sendmail`-style pipelines and `git send-email`
  (`sendmail_command = "mailbox send --stdin"`) work.
- The unified inbox is a view, never a rewrite: nothing mailbox does to a
  mailbox is invisible or unintelligible to another IMAP client.

## 14. Verification

End-to-end confidence needs a server in a known state, and the two things that
make that affordable are **symbolic state** and **cheap reset**.

Assertions never name a server-assigned value. Fixtures declare messages by
symbolic name; the seeder records whatever UID the server assigned (`UIDPLUS` on
`APPEND`, or a read-back where it is unavailable); tests refer to
`fixture["invoice-with-pdf"]`. A hardcoded `UID 4711` is a test that runs against
exactly one server, and provider divergence is precisely what we need to catch.

Reset is per-account, not per-container: one long-lived server, and each test
gets a fresh user created by copying a template Maildir — single-digit
milliseconds, and tests run in parallel. Deleting messages over IMAP to clean up
is both slower and a test of the wrong thing.

### 14.1 Fixtures

A fixture is a declarative manifest (headers, structure, attachments,
flags, folder, and an optional pinned `Message-ID` and `INTERNALDATE`) that
renders to bytes deterministically. The same manifest seeds the server, seeds a
local store directly for tests that need no server, and provides the expected
values. Generated corpora (the 1M-message perf set, §14.5) are produced from a
seeded generator and cached as a CI artifact rather than committed.

### 14.2 Levels

| Level | What runs | Speed | Gates PRs |
|---|---|---|---|
| **L0** | MIME parsing, JWZ threading, query parser, crypto vectors — corpus + fuzz, no server | µs | yes |
| **L1** | Recorded-transcript replay against the sync engine (VCR-style) | ms | yes |
| **L2** | Real Dovecot, Cyrus, Stalwart in containers, seeded from fixtures | ~1 s | yes |
| **L3** | Adversarial mock server: fault and latency injection | ms | yes |
| **L4** | Real Gmail, Outlook/Exchange Online, Fastmail, Yahoo accounts | slow, flaky | nightly, non-gating |

- **L1** transcripts are recorded from L2 and L4 runs and refreshed on a
  schedule; a stale transcript is a test that passes about a server that no
  longer exists, so each carries the date and server version it came from.
- **L3 is our own server**, in-tree, because real servers are too well-behaved
  to reach the recovery paths: truncated literals, a literal count that
  disagrees with its payload, `UIDVALIDITY` bumped mid-session, `BYE` during a
  `FETCH`, an `IDLE` that never speaks again, a `CAPABILITY` advertising
  `QRESYNC` that then rejects it, arbitrary latency and connection drops. Every
  recovery claim in §5.3 has a corresponding L3 scenario.
- **L4 detects provider drift, not regressions.** It is non-gating on purpose:
  a suite that goes red for reasons the author cannot fix trains everyone to
  ignore red suites.

Every end-to-end test drives the CLI and asserts on `--json` output, so the test
suite is the largest consumer of the agent contract (§6.4) and any breakage of
it fails a build rather than a user.

### 14.3 Oracles

Example-based tests will not find these three:

1. **Convergence.** Drive two instances through a randomized operation sequence
   against one account, sync both, and assert their `debug store-dump` outputs
   are identical and that the server agrees. This is the test of §4.1 and §5.3.
2. **Crash-only.** A supervisor `SIGKILL`s a process at a randomized trace
   event, restarts it, and asserts: no lost outbox entry, no duplicate send, no
   torn transaction, no store that fails to open. Nightly, looped. "No duplicate
   send" holds everywhere except the one window §16 accepts — a crash between
   the server taking `DATA` and the store recording it — which is the same
   ambiguity as the dropped `250` and is resolved the same way.
3. **The double-send window.** L3's submission side accepts `DATA` and drops the
   connection before `250`. This single fault is the entire justification for
   idempotency keys (§6.5) and gets a dedicated test rather than a property-test
   hope. Per §16 the outbox retries here and a duplicate delivery is accepted,
   so the test pins exactly that: the message is not lost, the caller's retry
   with the same key does not enqueue a second entry, and the duplicate the
   retry produces is asserted rather than wished away.

Pairing (§8) adds two more: known-answer vectors for the PAKE and the AEAD, and
a two-process end-to-end against an in-process relay including a MITM case that
must abort on SAS mismatch.

### 14.4 Robustness

MIME parsing, the query parser, and the pairing payload decoder are fuzzed
continuously; a corpus of real-world broken messages is kept as regression
tests. A malformed message may render badly; it may never crash, hang, or lose
data. The JSON contract has schema-conformance tests, and breaking it requires a
`mailbox/v2` schema version and a deprecation period.

### 14.5 Performance

The §10 budgets run against the generated 1M-message corpus on a dedicated
runner — shared CI runners are too noisy to gate on. Builds fail on regression
against the last known-good measurement (default: 10%), not on the absolute
budget, so the numbers stay honest instead of being retuned whenever they hurt.

## 15. Milestones

Each milestone names the verification levels (§14.2) that must be green before
it is done. A level, once required, stays required.

1. **M1 — core**: store (SQLite + FTS5), IMAP read sync, query, local JWZ
   threading, CLI `ls`/`show`/`search`, JSON contract, one account.
   *Verification:* **L0** (parser, threading, query corpora) and **L2** against
   Dovecot. The fixture format (§14.1), `--trace`, `sync --once`, and
   `debug store-dump` are part of this milestone's deliverable, not retrofitted
   — every later level is built on them.
2. **M2 — write**: flags, moves, SMTP + outbox, compose, multi-account, unified
   inbox.
   *Verification:* adds **L3** — writes are where fault injection starts to
   matter — including the double-send window (§14.3), and the crash-only oracle
   in its first form. L2 widens to Cyrus and Stalwart, since write paths are
   where servers diverge most.
3. **M3 — TUI** and desktop notifications; full-text search; offline mode.
   *Verification:* adds **L1** transcript replay (the engine is now stable
   enough for recordings to be worth keeping) and the §14.5 perf suite against
   the 1M corpus — search budgets are meaningless before full-text lands.
4. **M4 — pairing, offline transports only**: QR and LAN, the encrypted export
   file, `--merge`/`--replace`. No relay, no short code.
   *Verification:* **L0** known-answer vectors for the AEAD and the payload
   codec, plus the two-process pairing end-to-end with the MITM/SAS-mismatch
   case (§14.3). No new level, but pairing does not ship without these.
5. **M5 — GUI + web** (shared wasm UI), `mailbox serve`.
   *Verification:* the wasm build runs L0 in-browser; UI work is covered
   through the same CLI/JSON assertions rather than a screenshot suite.
6. **M6 — mobile**: iOS and Android on the same core; optional notifier.
   *Verification:* L0 + L2 must run on-device in CI — this is the milestone that
   would expose an FTS5 or store assumption that only held on desktop.
7. **M7 — agent polish**: `mcp`, `watch`, policy/approval queue, schema tooling.
   *Verification:* schema-conformance coverage (§14.4) becomes gating for every
   command, and the policy/approval rules get their own L3 scenarios (denied
   recipient, rate limit, retry after a mid-send failure).
8. **M8 — cross-device state**: the §5.4 state folder (tags, saved searches,
   shared snooze, address book), `mailbox devices` and revocation, and — after
   an outside review of the PAKE — the short code, the rendezvous relay, and
   `mailbox relay`.
   *Verification:* the convergence oracle (§14.3) becomes gating and grows to
   three instances; **L4** joins as a nightly suite, since the state folder is
   the feature most likely to be broken by a provider's folder semantics; the
   PAKE gets known-answer vectors and an outside review before the relay ships.

Snooze appears in M5 in its local-only form (§7) and becomes cross-device in M8.

## 16. Decisions

Recorded so they are not relitigated. Each names what would justify reopening it.

- **Search backend: SQLite FTS5, everywhere.** One file, one fsync domain, no
  second index to hold coherent with the store, and it runs unchanged on mobile.
  Tantivy is not planned. *Reopen if* full-text search misses its §10 budget at
  1M messages after tuning.
- **Cross-device state folder: default on, shipped late.** Default on wherever
  the server permits folder creation, but announced on first sync, renameable,
  and permanently disableable. It lands in M8; until then state is local and
  pairing carries a snapshot. *Reopen if* real servers make the folder
  disruptive in ways `SPECIAL-USE`-style hiding does not solve.
- **Threading: JWZ locally, always.** Server `THREAD` is unused even when
  advertised, because implementations disagree and a conversation must group
  identically on every device. *Reopen if* local threading misses the §10 budget
  on very large mailboxes.
- **Pairing relay: deferred to M8.** v1 is QR + LAN + export file. The relay and
  the short code wait on an outside review of the PAKE, since a 45-bit code
  guarded by an unreviewed handshake is a credential leak, not a convenience.
  *Reopen if* setup telemetry — which we do not collect — is replaced by actual
  user reports that camera-less, different-network pairing is common enough to
  hurt.
- **The double-send window: retry, and accept the duplicate.** When SMTP
  submission accepts `DATA` and the connection drops before the `250`, the
  client cannot know whether the message was queued. The outbox retries. That
  can deliver twice; the window is narrow, and a duplicate is a much smaller
  harm than a silently lost message. Idempotency keys (§6.5) still cover the
  other half — a caller re-invoking `mailbox send` gets the original result
  rather than a second delivery. *Reopen if* duplicates turn out to be common
  enough in practice to annoy, in which case the alternative is to mark the
  entry uncertain and require an explicit retry.

- **Snooze: local-only first, low priority.** A timer plus a local record; the
  message returns on the device that snoozed it. The UI states this rather than
  implying sync. Cross-device snooze rides the state folder in M8.

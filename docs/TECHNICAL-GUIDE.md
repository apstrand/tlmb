# Mailbox technical guide

This describes the current implementation, including a few places where it is
less ambitious than `SPEC.md`.

## Shape of the system

Mailbox is a Rust workspace with one mail core and several frontends:

```text
CLI / TUI / desktop webview / local web UI / iOS / hosted browser
                         │
                 mailbox-api types
                         │
        Workspace → one Session and Store per account
                         │
       IMAP sync + SMTP submit + MIME + query/threading
                         │
             SQLite + compressed MIME blobs
```

`mailbox-core` owns account configuration, credentials, MIME parsing, search,
threading, storage, IMAP, SMTP, the outbox, and pairing. `mailbox-api` contains
the serialized request and response types. The CLI calls the core directly.
The TUI does the same through a non-blocking backend. `mailbox serve` exposes a
small HTTP/JSON adapter, while `mailbox gui` puts those same HTML, CSS, and
JavaScript assets in a native Tauri webview. There is no frontend-specific mail
engine.

The hosted build is different only at the platform seam: the core is compiled
to WebAssembly and runs in a worker. SQLite is serialized to browser storage,
blob files use the same logical byte-store interface, and network streams go
through the configured relay because a browser cannot open raw IMAP or SMTP
sockets. The native iOS shell links the core through `mailbox-ffi` and keeps
credentials in Keychain.

## Local data and process ownership

Configuration defaults to `~/.config/mailbox/config.toml`. Each account gets a
directory below `~/.local/share/mailbox/` containing:

- `meta.db`: SQLite in WAL mode. It holds folders, message metadata and
  placements, flags, threads, pending server operations, outbox state, and an
  FTS5 index.
- `blobs/`: raw RFC 5322/MIME messages, zstd-compressed and addressed by a
  BLAKE3 hash. Identical bytes are stored once even if IMAP exposes the message
  in several folders.
- `state.toml`: IMAP progress such as UID validity and highest modification
  sequence.

Keeping headers and queries in SQLite makes listing and search local, fast, and
available offline. Keeping complete MIME separately avoids inflating the
database and gives large bodies atomic, idempotent writes. The cost is that one
logical update crosses SQLite and the blob directory; recovery relies on
content-addressed writes plus replay, not one filesystem-wide transaction.

Writable sessions take a per-account advisory `flock`. This prevents two sync
engines from interleaving IMAP state, blob writes, and queue replay. Read-only
commands skip that lock and use SQLite's WAL read path. The design document
describes forwarding writes to the owner over a Unix socket, but that part is
not implemented: a competing sync currently returns a conflict instead of
proxying the request. Local operations already committed to the queue are not
lost.

A `Workspace` opens one `Session` per selected account and merges their sorted
results. Stable message IDs include the account identity, so a write can be
routed back to its owning session without exposing IMAP UIDs as application
IDs.

## Receiving and reading mail

A sync resolves the credential, opens IMAP through rustls, authenticates with a
password or XOAUTH2 token, discovers folders and server capabilities, and then
updates each folder. QRESYNC is preferred, CONDSTORE is the next-best delta
path, and ordinary UID listing is the compatibility fallback. UIDVALIDITY
changes invalidate the relevant remote mapping and cause a safe rebuild.

Headers are fetched newest-first in chunks so a large account becomes useful
before its first complete scan. Bodies follow the account's offline policy:
recent mail is prefetched by default, opened bodies are fetched as needed, and
`sync --full` caches everything. The UI reports partial coverage rather than
presenting a partial local search as complete.

Incoming MIME is parsed defensively. Metadata and extracted searchable text go
to SQLite; the original bytes remain in the blob store. Conversation threads
are computed locally from `Message-ID`, `References`, and `In-Reply-To`, with a
bounded subject fallback. This is deterministic and works offline, at the cost
of sometimes differing from a provider's own grouping.

Flags and moves are optimistic. The store changes immediately and records an
operation for the next sync. The IMAP layer uses MOVE/UIDPLUS and other
extensions when advertised, with compatible fallbacks. IMAP folder
`SPECIAL-USE` roles decide what “archive”, “trash”, “junk”, and “sent” mean;
local preferences resolve duplicate roles.

## Sending mail

Compose produces a complete MIME message and a durable outbox row before any
network request. A flush opens SMTP lazily, authenticates, and submits each due
entry. A `250` response marks delivery complete. Temporary failures back off;
permanent failures remain inspectable. If the connection dies after the server
accepted the bytes but before the client saw the reply, delivery is uncertain
and retrying can duplicate mail. SMTP has no general transaction ID that can
remove that window.

Filing the Sent copy is a separate step. It uses IMAP APPEND and can retry
without resending the message; Gmail is exempt because its SMTP service files
the copy itself. This separation is why the UI says “queued”, not “sent”, when
compose closes.

## Security model

The primary trust boundary is the local OS account. Mail bodies and metadata
are **not encrypted at rest**. Native stores are created with restrictive Unix
permissions, but a process running as the user, root, a stolen unlocked disk,
or a backup reader can read them. Use full-disk or home-directory encryption if
that is in scope.

Credentials do not belong in `config.toml` or `meta.db`. Resolution order is
environment override, an in-process secret supplied by a native shell, a
configured credential command, then the OS keychain (`secret-tool` on Linux or
Keychain on macOS). OAuth access tokens are short-lived cache material; refresh
tokens use credential storage when possible. A credential command is trusted
code and runs with the user's privileges. Environment variables and command
stdout also remain visible to the usual same-user debugging and process
interfaces.

IMAP and SMTP require TLS by default. Certificate validation uses rustls and
the platform/native root set where available. Plaintext is accepted only when
explicitly configured for a loopback host. Autoconfiguration is useful input,
not a substitute for TLS authentication: published or guessed hostnames still
have to present a valid certificate.

The local web server binds loopback by default and uses a 256-bit bearer token.
The first URL exchanges it for an HttpOnly, SameSite=Strict cookie for page and
message-frame navigation; API requests require the token in a custom header.
CORS preflights are refused, comparisons are constant-time, request sizes and
concurrent connections are bounded, and account setup is disabled on a
non-loopback bind because `mailbox serve` is plain HTTP. Binding it to a LAN
still exposes already-synced mail and the bearer token to anyone able to sniff
that network; put a TLS reverse proxy and proper access control in front if the
LAN is not trusted.

HTML mail is sanitized, remote resources are disarmed by default, and the body
is rendered in a separate sandboxed iframe with no scripts, forms, or
same-origin privilege. Loading remote images is an explicit per-message or
per-sender choice and leaks the usual IP address and tracking identifier to the
sender. Links open outside the frame with no referrer, but remain ordinary
untrusted links.

Pairing transfers configuration and credentials under XChaCha20-Poly1305 with
version-bound associated data and HKDF-SHA-256 key separation. QR/LAN pairing
uses a random 256-bit secret. Typed codes use CPace, a balanced PAKE over
Ristretto255, then display a transcript-bound short authentication string.
Relay storage sees an opaque channel and encrypted payload. The CPace
composition is tested but has not received an external security review; treat
that as an assessment item when pairing is in scope.

Optional cross-device state is encoded as append-only assertions and encrypted
with a separate derived key before being stored as messages in a hidden IMAP
folder. The wire format and convergence logic exist, but deployment maturity
should be checked against the build being assessed rather than assumed from
the design spec.

Tracing redacts authentication commands and summarizes large IMAP literals.
That reduces accidental credential or body disclosure, but diagnostic output,
core dumps, swap, browser storage, and local backups remain part of the host's
security boundary.

## Operational assessment checklist

For a deployment review, check the effective account config, credential source,
TLS mode and certificate roots, store ownership and backup policy, whether the
web listener leaves loopback, relay ownership for hosted builds, and which
folders carry each special-use role. Exercise offline recovery, a UIDVALIDITY
reset, a temporary SMTP failure, and a killed sync with queued operations. Also
decide whether the unencrypted local cache and the unaudited pairing PAKE meet
the threat model; neither can be fixed by IMAP server policy.

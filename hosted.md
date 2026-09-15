     ╭───────────────────────────────────────────────────────────────────────────────────────────╮
     │ A hosted flavour: the core in the browser, no mailbox data on the site host               │
     │                                                                                           │
     │ Context                                                                                   │
     │                                                                                           │
     │ mailbox serve assumes the machine serving the page is the machine that owns               │
     │ the mail: the config, the SQLite store, the blobs and the keychain entry all              │
     │ live beside it. That is right when you run it on your own box, and wrong when             │
     │ someone else runs it for you — there the server holds every user's mail and               │
     │ every user's password.                                                                    │
     │                                                                                           │
     │ The ask is a second flavour where the site host holds no mailbox data per user:           │
     │ account settings, the credentials and the store live in the browser, with the             │
     │ existing core compiled to wasm so the experience is the same rather than a                │
     │ lesser copy of it. serve is the wrong entry point for that — it names a                   │
     │ server that, in this flavour, has no mailbox to serve. So this is a site                  │
     │ generator: a command that emits a static bundle you can host anywhere, plus               │
     │ one small relay component that exists only because of a browser limitation.               │
     │                                                                                           │
     │ This is a large project. Stage 0 below is a spike whose only job is to answer             │
     │ whether the architecture is possible at all; everything after it is contingent            │
     │ on that answer.                                                                           │
     │                                                                                           │
     │ The shape                                                                                 │
     │                                                                                           │
     │ Two products, one core, one document:                                                     │
     │                                                                                           │
     │ ┌──────────────┬────────────────────────┬───────────────────────────────────────────────┐ │
     │ │              │ mailbox serve (local)  │             mailbox site (hosted)             │ │
     │ ├──────────────┼────────────────────────┼───────────────────────────────────────────────┤ │
     │ │ Config       │ config.toml on disk    │ browser (IndexedDB)                           │ │
     │ ├──────────────┼────────────────────────┼───────────────────────────────────────────────┤ │
     │ │ Credentials  │ OS keychain            │ browser, never transmitted to the site's host │ │
     │ ├──────────────┼────────────────────────┼───────────────────────────────────────────────┤ │
     │ │ Store        │ <state_dir>/<account>/ │ SQLite-in-wasm, persisted to OPFS             │ │
     │ ├──────────────┼────────────────────────┼───────────────────────────────────────────────┤ │
     │ │ Core runs    │ in the server process  │ in a Web Worker in the tab                    │ │
     │ ├──────────────┼────────────────────────┼───────────────────────────────────────────────┤ │
     │ │ Server holds │ everything             │ the static files, and nothing else            │ │
     │ └──────────────┴────────────────────────┴───────────────────────────────────────────────┘ │
     │                                                                                           │
     │ The DOM client, the §6.4 contract and mailbox-core are shared. What differs               │
     │ is where the core runs and where its state lives.                                         │
     │                                                                                           │
     │ The seam already exists                                                                   │
     │                                                                                           │
     │ mailbox_ffi::Mailbox::exec(method, args_json) -> json                                     │
     │ (crates/mailbox-ffi/src/lib.rs:373) is already the whole core behind one JSON             │
     │ call — it is how iOS talks to it. A wasm build is that same façade with                   │
     │ wasm-bindgen instead of the C ABI, and the browser client's single choke                  │
     │ point is api() in crates/mailbox-web/web/mailbox.js:38. Hosted mode swaps                 │
     │ one function for the other; nothing above that line changes.                              │
     │                                                                                           │
     │ exec is missing identities, snooze/unsnooze and outbox, which the web                     │
     │ client uses. Those get added to the existing dispatch, which benefits iOS too.            │
     │                                                                                           │
     │ The one piece that cannot be static                                                       │
     │                                                                                           │
     │ A browser cannot open a TCP connection, so it cannot speak IMAP. Something has            │
     │ to bridge WebSocket to TCP. The important question is what that bridge gets to            │
     │ see. Payload content can be hidden: rustls is already a dependency and needs              │
     │ only a byte stream, so TLS is terminated inside the wasm module and the bridge            │
     │ carries ciphertext it cannot read. It never learns a password or parses IMAP.             │
     │ It still sees destinations, TLS metadata, timing and volume.                              │
     │                                                                                           │
     │ That makes it a separate, optional component (mailbox relay), which is the same           │
     │ shape §11 already uses for the push notifier, except that this one needs no mail           │
     │ credentials. A fixed-route relay can be stateless; arbitrary providers require an         │
     │ account and route broker. A site with no relay can be opened but cannot sync.             │
     │                                                                                           │
     │ The safe default is fixed destination routes, initially IMAP/TLS on port 993, plus        │
     │ per-connection byte, idle, lifetime and concurrency caps. The arbitrary-provider          │
     │ design and its additional account and traffic controls are explored below.                │
     │                                                                                           │
     │ Stages                                                                                    │
     │                                                                                           │
     │ Stage 0 — Feasibility spike (gates everything; do this first and report)                  │
     │                                                                                           │
     │ New crates/mailbox-wasm (cdylib, wasm-bindgen) wrapping                                   │
     │ mailbox_ffi::Mailbox. Get wasm32-unknown-unknown to build and answer three                │
     │ questions, each of which can sink the design:                                             │
     │                                                                                           │
     │ 1. SQLite. rusqlite's bundled feature compiles SQLite C with cc.                          │
     │    Does it build for wasm32, and does an in-memory database run our schema                │
     │    (store/schema.rs) including FTS5?                                                      │
     │ 2. TLS. Does rustls (currently on ring) complete a client handshake in                    │
     │    wasm against a real IMAP server over a WebSocket byte stream? ring's                   │
     │    wasm32 support is partial; a provider swap may be needed for that target.              │
     │ 3. Everything else. zstd (C, used by store/blobs.rs), blake3,                             │
     │    mail-parser, imap-proto, argon2, time, getrandom (needs its js                         │
     │    backend).                                                                              │
     │                                                                                           │
     │ Host-only modules get #[cfg(not(target_arch = "wasm32"))]: notify, open,                  │
     │ owner, attach's extraction, credentials' keychain and command paths,                      │
     │ imap/watch's threads, dns, pair/transport.                                                │
     │                                                                                           │
     │ Deliverable: a throwaway page that opens an in-memory store, runs an L0 query,            │
     │ and completes a TLS handshake through a relay — or a written account of which             │
     │ of the three blocked and what it would cost to unblock.                                   │
     │                                                                                           │
     │ Stage 1 — Blocking I/O without threads                                                    │
     │                                                                                           │
     │ The IMAP client is blocking by design (imap/connection.rs, and §4.1 depends               │
     │ on it). Rewriting the sync engine async would be enormous, so instead the core            │
     │ runs in a dedicated Web Worker and blocks on a SharedArrayBuffer bridge                   │
     │ that the main thread pumps into a WebSocket — the same trick SQLite's OPFS                │
     │ layer uses. Transport (imap/connection.rs:62) gains a third variant behind                │
     │ cfg(wasm32); Read/Write are already the only interface above it.                          │
     │                                                                                           │
     │ This needs the page to be cross-origin isolated (COOP/COEP), which the                    │
     │ generated site's headers must declare and which any static host must be able to           │
     │ send — a documented requirement of hosting a generated site.                              │
     │                                                                                           │
     │ Stage 2 — A storage seam in the core                                                      │
     │                                                                                           │
     │ Today store/blobs.rs writes files and store/mod.rs opens meta.db by path;                 │
     │ config, history, policy, oauth and trace each reach for std::fs.                          │
     │ Introduce one trait for "where bytes live", with the host implementation being            │
     │ exactly today's behaviour, and a wasm implementation over OPFS. SQLite starts             │
     │ in memory with periodic serialisation to OPFS (simplest thing that works), with           │
     │ the seam placed so a real OPFS VFS can replace it later without touching                  │
     │ callers. This is where the "partial sync" option you mentioned would land.                │
     │                                                                                           │
     │ Stage 3 — mailbox site                                                                    │
     │                                                                                           │
     │ mailbox site --out ./dist [--relay wss://relay.example/] writes the page,                 │
     │ stylesheet, client, wasm module and a small config.json. The client picks its             │
     │ backend from that config: fetch("/api/…") when served by serve, the wasm                  │
     │ worker when generated. Message bodies, which today are an iframe pointing at              │
     │ /body?id= (mailbox.js:334), become blob: URLs produced by wasm under the                  │
     │ same sandbox and CSP.                                                                     │
     │                                                                                           │
     │ Setup in hosted mode reuses the form built last week: it writes to IndexedDB and          │
     │ calls the wasm accounts.add instead of POST /api/accounts, and the Test                   │
     │ button becomes a wasm accounts.test through the relay.                                    │
     │                                                                                           │
     │ Stage 4 — mailbox relay                                                                   │
     │                                                                                           │
     │ The blind pipe described above, with its limits, plus its refusal to run                  │
     │ without TLS in front of it. Fixed provider routes are the default; arbitrary              │
     │ providers add the account broker and TLS-aware gate described below.                      │
     │                                                                                           │
     │ Stage 5 — Tests                                                                           │
     │                                                                                           │
     │ - L0 in wasm, which restores SPEC §15's "the wasm build runs L0 in-browser" —             │
     │   TODO.md:76 records that this clause lost its wasm build when the UI became              │
     │   DOM.                                                                                    │
     │ - Relay tests against the in-tree mailbox_l3::ImapServer.                                 │
     │ - A headless browser run of the generated site, which TODO.md:82 lists as                 │
     │   still missing on the host.                                                              │
     │                                                                                           │
     │ Spec amendments                                                                           │
     │                                                                                           │
     │ §3's platform table gains the hosted flavour and says plainly that the site host          │
     │ holds no mailbox data. §16 gains a decision entry: the UI stays DOM (the existing         │
     │ entry is about not drawing a canvas, and this does not reopen it), while the              │
     │ core compiles to wasm for this flavour only — with "reopen if" naming the                 │
     │ spike's three answers.                                                                    │
     │                                                                                           │
     │ Risks, honestly                                                                           │
     │                                                                                           │
     │ - Stage 0 questions 1 and 2 are real unknowns. If TLS-in-wasm proves                      │
     │   impractical, the fallback is a relay that terminates TLS — which sees                   │
     │   passwords, and would make "the server stores nothing" true but "the server              │
     │   sees nothing" false. That is a materially different product and worth a                 │
     │   decision rather than a workaround.                                                      │
     │ - A browser-held store means a lost browser profile is a lost mailbox. Export             │
     │   and re-sync need to be obvious in the hosted client.                                    │
     │ - Binary size: §10 budgets 25 MB stripped for the native binary; a wasm module            │
     │   has its own budget and SQLite plus rustls is not small.                                 │
     │                                                                                           │
     │ Verification                                                                              │
     │                                                                                           │
     │ - Stage 0: cargo build -p mailbox-wasm --target wasm32-unknown-unknown, then              │
     │   the throwaway page in Chrome via the browser tools.                                     │
     │ - Per stage: cargo test --workspace stays green, and mailbox serve keeps                  │
     │   behaving exactly as it does today — local mode must not regress, and the                │
     │   serving tests in crates/mailbox-cli/tests/serving.rs are the guard.                     │
     │ - End to end: generate a site, host it with any static server, add an account             │
     │   through the form against the L3 IMAP server behind a local relay, and read a            │
     │   message — with the server's filesystem observably untouched.                            │
     │                                                                                           │
     │ Proposal                                                                                  │
     │                                                                                           │
     │ Do Stage 0 only, then report back with what it found before committing to the             │
     │ rest. It is a day or two of work and it decides whether the other stages are              │
     │ worth planning in detail.                                                                 │
     ╰───────────────────────────────────────────────────────────────────────────────────────────╯

## Fixed providers and arbitrary IMAP

The safe hosted default is a finite provider allow-list. The relay exposes
named WebSocket routes such as `/own` and `/google`; each route maps to a
destination configured at startup. The browser can select a route, but cannot
send a host or port. This supports a small set of known providers while
keeping the relay a blind, bounded pipe.

An arbitrary-IMAP service cannot safely accept an unauthenticated `HOST:PORT`
from the browser. Even though the relay cannot decrypt the TLS payload, it
would still be an internet-wide TCP proxy and could be abused for scanning,
credential attacks, or connections to internal addresses. SMTP submission
makes the consequences larger: the upstream still has to accept the user's
SMTP credentials, so this is not an unauthenticated mail relay, but it can
hide the source of password attacks and abusive authenticated sending.

There are two useful lines of defence. They complement one another, but
neither turns an arbitrary ciphertext pipe into something intrinsically safe.

## Relay accounts: a policy handle, not proof of good intent

Making the relay an account service changes the economics and gives the
operator somewhere to attach limits and revocation. A verified address is a
small amount of friction; a passkey-bound account is harder to steal; payment
adds cost to creating replacements and gives the operator a dispute trail.
Payment is therefore useful even if the price only covers bandwidth. It is
not identity proof, though: prepaid cards, compromised accounts, chargebacks,
and a determined abusive customer still exist. The security argument must be
"one accountable principal gets a small, stable capability", not "a customer
would not abuse it".

The useful assumption is the second one: a real mailbox user changes mail
servers rarely. Model a relay account as one or a few registered
destinations, rather than permission to name a destination on every
connection:

1. During setup, the user proposes `hostname:port`. The broker resolves it
   itself, rejects every non-public address in every answer, applies the port
   policy, and records the hostname and a pinned public address set. It repeats
   those checks when DNS is refreshed; it never follows a client-supplied IP.
2. A sensitive account action registers or changes that destination. Require
   fresh authentication, notify the user, limit the number of destination
   slots, and put changes behind a cooldown and a low rolling limit. Ordinary
   reconnects to an existing destination stay cheap.
3. The browser obtains a short-lived, single-destination connection ticket.
   The signed ticket names the account, registered target, expiry and resource
   limits. The TCP relay accepts the ticket and chooses the destination; the
   WebSocket request never contains an arbitrary host or IP.
4. Enforce quotas per account as well as per IP: concurrent connections,
   connection attempts, bytes, sustained bandwidth, connection lifetime and
   monthly transfer. Keep global and per-destination circuit breakers, since
   many paid accounts can still attack one host.
5. Keep enough operational history to revoke abusive accounts: destination
   changes, resolved/pinned addresses, connection times, byte counts and
   network outcomes. Do not log payload bytes. State the retention period and
   make it part of the privacy description.

This is an authenticated broker in front of a mostly stateless data plane.
It need not provision one process per user; route tickets provide the same
isolation more cheaply. It does, however, mean the relay service is no longer
stateless. The precise promise becomes: the static-site host has no account or
mailbox data; the relay has account, billing, route and usage metadata, but no
mail credentials, message contents or browser store. Those roles may be run
by different operators.

Origin checks and an API token stored in JavaScript are not sufficient. An
attacker can ignore the generated site and speak WebSocket directly. The
relay must authenticate every connection itself, preferably with a
short-lived ticket obtained using a device-bound key or passkey. IP limits
remain useful for floods but cannot stand in for an account boundary.

Accounts make a deliberately narrow arbitrary-provider offering plausible:
for example, one IMAP target on port 993, one infrequently changed SMTP target
on 465, and conservative quotas. They do not justify exposing a generic
`CONNECT host:port` API. A paid account should still be unable to scan a
subnet, rotate across thousands of public hosts, or reach a newly rebound
private address.

A natural product split follows from this. Anonymous or trial use gets only
curated provider routes. An identified, possibly paid, relay account can
register an arbitrary IMAP service under the low-churn policy. SMTP is a
separate capability enabled later, with lower limits and its own abuse switch;
paying for IMAP should not automatically grant it. Provider discovery can
also reduce the setup attack surface: targets obtained from a domain's
standard mail configuration or a known-provider catalogue can be admitted
cheaply, while a manual hostname consumes the account's limited change
budget. This is friction, not proof that the user owns a mailbox there—the
blind relay cannot observe a successful login.

## What the relay can understand without seeing mail

"Encrypted" does not mean "unclassifiable". With TLS terminated in the wasm
client, the relay can observe:

- the relay account and registered logical route;
- the destination hostname selected by the broker, its resolved IP and port;
- connection success or failure, timings, duration, byte totals, direction
  and rough transfer shape; and
- the beginning of the TLS handshake, including the record format and,
  normally, the ClientHello's SNI and offered TLS versions. Encrypted Client
  Hello would hide SNI and should be rejected if matching SNI is part of the
  policy.

That is enough for a useful TLS-aware gate. Before forwarding application
bytes, require a syntactically valid ClientHello within a small byte/time
budget, require its SNI to equal the registered hostname, reject unexpected
TLS versions, and pin the outbound connection to the address approved by the
broker. Once the handshake has begun, apply byte, rate, idle and lifetime
limits and look for operational anomalies: high target churn, bursts of short
connections, repeated TCP/TLS failures, unusual fan-out across accounts, or
traffic far outside normal mailbox synchronization envelopes.

These checks materially reduce scanning and use as a generic cleartext TCP
proxy. They can also reject accidental non-TLS connections. They do not reveal
whether the encrypted operation is `LOGIN`, `FETCH`, `APPEND` or SMTP `DATA`;
in TLS 1.3 even the server certificate is encrypted after ServerHello. The
relay consequently cannot count failed IMAP logins, inspect recipients,
distinguish a large attachment from bulk extraction, or prove that a client
is mailbox rather than custom code. Traffic shape is a signal for throttling
and review, not a protocol firewall.

Ports 143 and 587 deserve special treatment. They normally begin with a
plaintext IMAP/SMTP greeting and `STARTTLS`, so the current claim that the
relay carries ciphertext from the first byte is only true for implicit-TLS
ports 993 and 465. Supporting STARTTLS requires the wasm client to conduct
that plaintext prelude. The relay can validate a very small state machine for
the greeting, capability/EHLO and `STARTTLS`, then require a matching TLS
ClientHello, but it must not accept arbitrary plaintext while waiting for the
upgrade. The simpler first hosted service should allow only IMAP-over-TLS on
993; add 465, 143 or 587 only when their need and parser are explicit.

There is a hard boundary here: inspecting IMAP or SMTP commands requires
access to the plaintext. The practical way to get it is for the relay to
terminate TLS and establish a second TLS connection upstream. It can then
enforce the protocol precisely, but it also sees reusable passwords or OAuth
tokens, correspondents, subjects and message bodies. Selective TLS key
disclosure or trusting that the published browser code is the code connecting
does not remove this boundary; a hostile user controls their browser and can
write another client.

The viable service shapes are therefore:

| Shape | Arbitrary providers | Relay state | What the relay sees | Main control |
|---|---:|---:|---|---|
| Curated named routes | No | configuration only | destination and traffic metadata | fixed destinations |
| Account-bound route broker | Yes, narrowly | accounts, routes, billing, usage | destination and traffic metadata | stable capabilities, quotas, revocation |
| TLS-aware account broker | Yes, narrowly | same as above | metadata plus ClientHello | stable capabilities plus TLS-shape checks |
| TLS-terminating connector | Yes | accounts and connector state | credentials and mailbox traffic | full protocol policy |

The recommended hosted shape is the third row, initially restricted to port
993. It preserves the important content-confidentiality property while using
accounts, stable destinations and observable TLS metadata as layered abuse
controls. It should be described as a constrained, accountable relay, not a
stateless or abuse-proof one. If operation shows that metadata and quotas are
insufficient—especially before enabling SMTP—the honest choices are to keep a
curated provider list or explicitly adopt the TLS-terminating connector and
its different privacy promise.

Before Stage 4 becomes a public service, the remaining product decisions are
therefore measurable ones rather than architectural unknowns: what counts as
an identified account, whether payment is required before a manual target,
how many target slots and changes normal multi-account use actually needs,
what metadata retention is acceptable, and whether SMTP is worth the much
larger abuse surface. Start with tight values and relax them from observed
legitimate use; do not start broad and depend on traffic classification to
recover the boundary later.

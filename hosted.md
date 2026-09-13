     ╭───────────────────────────────────────────────────────────────────────────────────────────╮
     │ A hosted flavour: the core in the browser, nothing on the server                          │
     │                                                                                           │
     │ Context                                                                                   │
     │                                                                                           │
     │ mailbox serve assumes the machine serving the page is the machine that owns               │
     │ the mail: the config, the SQLite store, the blobs and the keychain entry all              │
     │ live beside it. That is right when you run it on your own box, and wrong when             │
     │ someone else runs it for you — there the server holds every user's mail and               │
     │ every user's password.                                                                    │
     │                                                                                           │
     │ The ask is a second flavour where the server holds nothing per user: the                  │
     │ account settings, the credentials and the store live in the browser, with the             │
     │ existing core compiled to wasm so the experience is the same rather than a                │
     │ lesser copy of it. serve is the wrong entry point for that — it names a                   │
     │ server that, in this flavour, has no mailbox to serve. So this is a site                  │
     │ generator: a command that emits a static bundle you can host anywhere, plus               │
     │ one small stateless component that exists only because of a browser limitation.           │
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
     │ see, and the answer can be nothing: rustls is already a dependency and                    │
     │ needs only a byte stream, so TLS is terminated inside the wasm module and the             │
     │ bridge carries ciphertext it cannot read. It never learns a password, never               │
     │ parses IMAP, and stores nothing — it is a pipe.                                           │
     │                                                                                           │
     │ That makes it a separate, optional, stateless component (mailbox relay),                  │
     │ which is the same shape §11 already uses for the push notifier, except that               │
     │ this one needs no credentials at all. A generated site is configured with a               │
     │ relay URL; a site with none can still be opened and set up, but cannot sync.              │
     │                                                                                           │
     │ Without hard limits a relay is an open proxy, so it ships with: a destination             │
     │ port allowlist (143/993/465/587), an optional destination host allowlist, a               │
     │ per-connection byte cap, an idle timeout, and a concurrent-connection cap.                │
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
     │ without TLS in front of it.                                                               │
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
     │ §3's platform table gains the hosted flavour and says plainly that the server             │
     │ holds nothing. §16 gains a decision entry: the UI stays DOM (the existing                 │
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

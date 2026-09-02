# mailbox

A self-contained IMAP & SMTP client. One Rust core, many faces: CLI, TUI, GUI,
web, and native mobile. See [SPEC.md](SPEC.md) for the design; this file
describes what is *built*.

## Status: M1 in progress

[§15](SPEC.md) sets M1 as: store (SQLite + FTS5), IMAP read sync, query, local
JWZ threading, CLI `ls`/`show`/`search`, JSON contract, one account —
verified by **L0** and **L2 against Dovecot**, with the fixture format,
`--trace`, `sync --once`, and `debug store-dump` shipped as part of it rather
than retrofitted.

### What works

| Area | State |
|---|---|
| Store: SQLite + FTS5, blob CAS, `state.toml` | done (§5.1) |
| Stable message id | done (§5.2) |
| IMAP read sync: `QRESYNC`, `CONDSTORE`, full-listing fallback | done (§5.3) |
| Lazy body fetch, prefetch window, `sync --full` | done (§5.1) |
| Local JWZ threading, narrowed subject grouping | done (§5.6) |
| Query language, with unknown keys as errors | done (§6.2) |
| `ls`, `show`, `search`, `folders`, `accounts`, `sync`, `status`, `schema` | done (§6.1) |
| `folders --remote` — what the server holds, without syncing | done (§6.1) |
| Optimistic writes: `flag`, `mv`, `cp`, `rm`, `archive`, `spam` | done (§5.3, M2) |
| Operation queue with idempotent replay, backoff, and conflicts | done (§5.3, M2) |
| SMTP submission, durable outbox, `Sent` upload | done (§5.5, M2) |
| `send`, `compose`, `reply`, `outbox` | done (§6.1, M2) |
| Idempotency keys, approval queue, `policy.toml` | done (§6.5, M2) |
| L3 adversarial SMTP server and its fault matrix | done (§14.2, M2) |
| Multi-account and the unified inbox | done (§1.5, §7, M2) |
| Crash-only oracle: `MAILBOX_CRASH_AT` and its supervisor | done (§14.3, M2) |
| L3 adversarial IMAP server and the §14.2 recovery scenarios | done (§14.2, M2) |
| L1 transcript record and replay | done (§14.2, M3) |
| Generated 1M corpus and the §10 performance suite | done (§14.5, M3) |
| JSON contract: `--json`, `--ndjson`, `--fields`, cursors, exit codes | done (§6.4, §6.6) |
| `--trace`, `sync --once --until`, `debug store-dump`, `debug fixture` | done (§6.7) |
| Fixture format | done (§14.1) |
| L0 and L2 suites | done (§14.2) |

### In progress: M2

Everything M2 names is built: flags, moves, SMTP and the outbox, compose,
multi-account, and the unified inbox — with L3 on both sides, the double-send
test, and the crash-only oracle.

One verification item is outstanding: **L2 has not been widened to Cyrus and
Stalwart**, and the existing Dovecot suite is currently skipping, because the
Docker daemon on this machine is not reachable from this user. Everything else
runs without it.

### Not built yet

- **OAuth2 and `accounts add`.** Autoconfig discovery and the system-browser
  PKCE flow.
- **Instance coordination.** The owner lock and the Unix socket of §4.1 are
  designed but not built; today a write command opens the store directly.
  Reads already take the read-only path, which is the half that matters most.
- **Account setup.** `accounts add`, autoconfig discovery, and OAuth2 are M2.
  An account is configured by hand (below), and credentials come from
  `credential_command`, the OS keychain via its own CLI, or an environment
  variable.
- **`IDLE` and a background loop.** `sync --once` is the only mode.
- **`COMPRESS=DEFLATE`**, `raw:` server-side search passthrough, and tags.

## Building

```sh
cargo build --release      # target/release/mailbox
```

No runtime, no daemon, no database server. SQLite is compiled in.

## Adding an account

`accounts add` and autoconfig discovery are M2. For now an account is a stanza
in `$XDG_CONFIG_HOME/mailbox/config.toml` (default
`~/.config/mailbox/config.toml`); everything is optional except the address and
the server ([§12](SPEC.md)):

```toml
[accounts.work]                 # `work` is the id `--account` takes
address    = "peter@example.com"
imap_host  = "imap.example.com"
imap_port  = 993                # 993 implies TLS, 143 implies STARTTLS
offline    = "recent"           # none | recent | full — the §5.1 body prefetch
```

**When the login name is not the address**, set `imap_user`. Shared mailboxes,
`DOMAIN\user` logins, and accounts whose address is an alias all need this;
without it the address is sent as the username.

```toml
[accounts.work]
address    = "team@example.com"     # what the mail says
imap_user  = "peter"                # what the server is told
identities = ["peter@example.com"]  # extra addresses `to:me` matches
```

`mailbox accounts` shows the login name in its own column when it differs from
the address, and `accounts test` prints `logged in as:` — because "wrong
username" and "wrong password" fail identically from the outside, and this is
the half you can check.

The password never goes in this file (§9). It is resolved, in order, from:

1. `MAILBOX_PASSWORD_<ID>` — the *account id* uppercased, with `-` and `.` as
   `_` (`[accounts.work]` → `MAILBOX_PASSWORD_WORK`), not the login name. Also
   `MAILBOX_OAUTH_TOKEN_<ID>` for a bearer token.
2. `credential_command` — anything that prints the secret on stdout:
   `pass show mail/example`, `gpg -dq ~/.mail.gpg`, `op read op://…`.
3. The OS keychain, through its own CLI:
   `secret-tool store --label=mailbox service mailbox account work` on Linux, or
   an internet password matching `imap_host` and the login user on macOS.

Check it before syncing — this connects, authenticates, and reports which of
§5.3's fast paths the server offers:

```sh
mailbox accounts            # what is configured, and whether a credential resolves
mailbox accounts test       # connect and authenticate for real
```

Then:

```sh
mailbox folders --remote     # what is on the server, before syncing anything
mailbox sync --once
mailbox ls
mailbox search 'from:alice has:attachment after:-7d'
mailbox show ^ --format=md --strip-quotes
mailbox --json flag ^ +seen  # instant locally, pushed on the next sync
mailbox archive ^
```

### Sending

Outgoing mail goes to a durable outbox first and only then to SMTP submission
(§5.5), so a queued message survives a crash, a restart, and a week offline.

```sh
mailbox send --to alice@example.com --subject Hi --body 'Hello.'
mailbox reply ^ --all
mailbox compose --to alice@example.com    # opens $EDITOR
mailbox outbox list
mailbox outbox flush                      # or just `mailbox sync`
```

Add SMTP settings to the account stanza:

```toml
smtp_host = "smtp.example.com"
smtp_port = 587                 # 587 implies STARTTLS, 465 implies TLS
# smtp_user = "peter"           # only when it differs from imap_user
```

`--delay 30` holds a message for thirty seconds so `mailbox outbox cancel <id>`
can stop it. `--dry-run` prints the exact bytes and touches no network.

For agents, `policy.toml` beside `config.toml` sets the guardrails of §6.5:

```toml
[send]
allow_recipients = ["*@example.com"]
deny_recipients  = ["ceo@example.com"]
max_recipients   = 10
max_per_hour     = 20
confirm_external = true
internal_domains = ["example.com"]
```

Under `MAILBOX_AGENT=1`, `send` holds the message for approval and returns
`{"status":"pending_approval","id":…}`; a human releases it with
`mailbox outbox approve <id>`. `--idempotency-key` makes a retried `send`
return the original result instead of queueing a second copy.

`folders --remote` asks the server instead of reading the local store: it lists
every folder with its message and unread counts, fetches nothing, and writes
nothing — so it works on an account that has never synced. `folders` without it
reads the local store, which is empty until the first sync.

### Several accounts

Add another `[accounts.*]` stanza and they become one mailbox: `ls` merges them
newest-first, rows gain an account column, and `account:home` splits them again.
`--account work` narrows any command to one, and `unified_inbox = false` under
`[general]` makes that the default.

Writes route themselves — a message id names its account (§5.2), so
`mailbox archive <work-id> <home-id>` reaches both servers and resolves each
one's own archive folder. `send --from` picks the account that owns the
identity.

To keep a test account away from your real one, set `MAILBOX_CONFIG` and
`MAILBOX_STATE_DIR`: they relocate every path the process touches (§6.7), so
nothing lands in `$HOME`.

```sh
export MAILBOX_CONFIG=/tmp/mb/config.toml MAILBOX_STATE_DIR=/tmp/mb/state
```

### Against the test server

The L2 Dovecot accepts any username with the password `testpass`, which makes
it the fastest way to get a real mailbox to poke at:

```sh
docker build -t mailbox-test-dovecot tests/dovecot
docker run -d --name mailbox-l2 -p 21430:143 mailbox-test-dovecot

mkdir -p /tmp/mb && cat > /tmp/mb/config.toml <<'TOML'
[accounts.dev]
address       = "me@example.com"
imap_host     = "127.0.0.1"
imap_port     = 21430
imap_security = "none"
require_tls   = false           # §9 allows plaintext only for a loopback host
TOML

export MAILBOX_CONFIG=/tmp/mb/config.toml MAILBOX_STATE_DIR=/tmp/mb/state
export MAILBOX_PASSWORD_DEV=testpass
mailbox accounts test
```

It starts empty. `mailbox debug fixture render <manifest>` emits NDJSON with
each message's bytes, folder, and flags, ready to `APPEND` — that is how the L2
suite seeds it (§14.1).

## For agents

Every command takes `--json` and emits one document stamped
`"schema": "mailbox/v1"`. Failures print
`{"error":{"code":…,"message":…,"retryable":…,"hint":…}}` to stderr and set a
distinct exit code (§6.6). `mailbox schema <command>` prints the JSON Schema,
and the test suite validates real output against it.

```sh
MAILBOX_AGENT=1 mailbox ls --json --fields id,subject,from
```

Under `MAILBOX_AGENT=1`, any state-changing command needs an explicit `--yes`.

## Testing

```sh
cargo test -p mailbox-api -p mailbox-core     # L0: µs, no server
cargo test -p mailbox-cli --test contract     # the JSON contract, no server
cargo test -p mailbox-cli --test l3_smtp      # L3: SMTP faults, no Docker
cargo test -p mailbox-cli --test l3_imap      # L3: IMAP faults, no Docker
cargo test -p mailbox-cli --test l1_replay    # L1: transcript replay, no server
cargo test -p mailbox-core --test perf        # §14.5: query plans, no dedicated runner
cargo test -p mailbox-cli --test crash_only   # the crash-only oracle, no Docker
cargo test -p mailbox-cli --test multi_account # several accounts, no Docker
cargo test -p mailbox-cli --test l2_dovecot   # L2: a real Dovecot in Docker
```

L1 replays a recorded IMAP conversation against the sync engine with no server
at all. `MAILBOX_RECORD=<scenario> MAILBOX_RECORD_PATH=<file> mailbox sync`
records one from whatever server is in front of it; `mailbox debug replay <file>`
plays it back. Recording starts *after* authentication, so a transcript is the
sync conversation and carries no credential — and replay checks what the client
**sent**, not just what it read, because a replay that only fed responses back
would let the engine send anything at all and still pass.

The committed corpus lives in `crates/mailbox-cli/tests/transcripts/` and is
re-recorded with:

```sh
cargo test -p mailbox-cli --test record_corpus -- --ignored
```

Those recordings came from the in-tree L3 server, because Docker is not reachable
here; §14.2 wants them from real servers, and re-recording against Dovecot is a
one-command job once it is.

### Performance

`mailbox debug generate --messages 1000000` fills a store from a seeded,
deterministic generator (§14.1), and `mailbox debug bench` measures the §10
budgets against it. Measured on this machine at a million messages:

| Operation | Measured | §10 budget |
|---|---|---|
| cold `mailbox ls` | 2.6 ms | 100 ms |
| structured search | 0.9 ms | 30 ms |
| full-text search, first page | 12 ms | 100 ms |
| open a cached thread | 0.02 ms | 50 ms |
| peak RSS for a listing | 9.7 MB | 80 MB |
| binary, stripped | 7.8 MB | 25 MB |

The suite that runs in CI gates on **query plans**, not timings: a plan is
deterministic and machine-independent, and §14.5 is explicit that shared runners
are too noisy to gate on. That is not a weaker test — the regression it caught
was `ls` falling back to a full table scan and a full sort, which no millisecond
threshold on a shared runner would have caught reliably.

`MAILBOX_PERF_MESSAGES=1000000 MAILBOX_PERF_BUDGETS=1` runs the real thing on a
dedicated runner.

`MAILBOX_CRASH_AT=<event>[:n]` kills the process at a named trace event with no
cleanup at all, which is what turns §14.3's crash-only oracle into a supervisor
loop: send a message, kill the flush at every event a send emits in turn,
restart, and check that no outbox entry was lost, no transaction was torn, the
store still opens, and nothing was delivered twice — except in the one window
§16 accepts, which is checked by asking the server how many copies it held
*before* the restart rather than by naming events.

L3 (`crates/mailbox-l3`) is our own pair of servers, in-tree, because real
servers are too well-behaved to reach the recovery paths — no MTA will accept a
message and then hang up before saying so, and no IMAP server will announce a
literal and send half of it. Both bind loopback on an ephemeral port. The SMTP
side records every message it accepts, which is the only way to prove a retry
did or did not duplicate; the IMAP side injects truncated literals, a literal
count that disagrees with its payload, a `UIDVALIDITY` bump mid-pass, `BYE`
during a `FETCH`, a `QRESYNC` advertised and then refused, latency, and
connection drops at every point in the conversation. No test sends anywhere
real.

`cargo test` alone runs all three.

**L2 needs nothing but a reachable Docker.** The harness builds
`tests/dovecot` on first use, starts it as a container named `mailbox-l2` on
port 21430, and reuses it on later runs — so the first invocation takes a
minute and the rest take seconds. Each test mints a fresh account rather than
cleaning up after itself (§14's "cheap reset"), so tests run in parallel and
leave the container reusable.

```sh
cargo test -p mailbox-cli --test l2_dovecot                    # the whole suite
cargo test -p mailbox-cli --test l2_dovecot -- --nocapture     # with the skip notes
cargo test -p mailbox-cli --test l2_dovecot an_expunge         # one test
docker rm -f mailbox-l2                                        # start over
```

Without Docker the suite skips itself and says so. `MAILBOX_L2_REQUIRED=1` turns
that skip into a failure — a suite that silently skips is a suite nobody
notices has stopped running — and CI sets it.

Nothing in the suite depends on wall-clock time or on a server-assigned value:
`MAILBOX_CLOCK` and `MAILBOX_SEED` pin the clock and the RNG (§6.7), and
fixtures name messages symbolically (§14.1).

## Layout

```
crates/mailbox-api    the JSON contract — types only, no logic
crates/mailbox-core   store, sync, query, threading, MIME
crates/mailbox-cli    the `mailbox` binary
tests/dovecot         the L2 server, configured in-tree
```

If a frontend needs logic that isn't in `mailbox-core`, that logic belongs in
`mailbox-core` (§4).

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
| JSON contract: `--json`, `--ndjson`, `--fields`, cursors, exit codes | done (§6.4, §6.6) |
| `--trace`, `sync --once --until`, `debug store-dump`, `debug fixture` | done (§6.7) |
| Fixture format | done (§14.1) |
| L0 and L2 suites | done (§14.2) |

### What M1 does not have yet

- **Writes.** Flags, moves, compose, SMTP, and the outbox are M2. Folders are
  opened with `EXAMINE`, so a sync can never set `\Seen` as a side effect.
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
mailbox sync --once
mailbox ls
mailbox search 'from:alice has:attachment after:-7d'
mailbox show ^ --format=md --strip-quotes
```

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
cargo test -p mailbox-cli --test l2_dovecot   # L2: a real Dovecot in Docker
```

`cargo test` alone runs all three.

**L2 needs nothing but a working Docker.** The harness builds
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

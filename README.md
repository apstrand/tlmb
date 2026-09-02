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

## Configuration

`$XDG_CONFIG_HOME/mailbox/config.toml`, everything optional except an account's
address and server ([§12](SPEC.md)):

```toml
[accounts.work]
address    = "peter@example.com"
imap_host  = "imap.example.com"
imap_port  = 993
# `pass`, `gpg`, `op read`, … — anything that prints the password on stdout.
# The secret never lives in this file (§9).
credential_command = "pass show mail/example"
offline = "recent"          # none | recent | full
```

Then:

```sh
mailbox sync --once
mailbox ls
mailbox search 'from:alice has:attachment after:-7d'
mailbox show ^ --format=md --strip-quotes
```

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

L2 builds and starts `tests/dovecot` on first use and reuses the container
afterwards. Without Docker it skips loudly; `MAILBOX_L2_REQUIRED=1` makes the
skip a failure, which is what CI sets.

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

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
| Newest-first chunked backfill, and saying when a mailbox is partial | done (§5.1) |
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
| Offline mode, lazy bodies, `attach` | done (§5.1, §6.1, §9, M3) |
| Desktop notifications | done (§11, M3) |
| TUI: mutt keys, incremental search, `keys.toml` | done (§7, M3) |
| Pairing: QR, LAN, encrypted export file | done (§8, M4) |
| `accounts add`/`edit`/`remove`, built-in provider table | done (§9) |
| OAuth2: PKCE, loopback redirect, refresh, `XOAUTH2` | done (§9) |
| Snooze, local-only (§7) | done (M5) |
| Background sync: `IDLE`, poll fallback, one owner | done (§5.3, §4.1) |
| Compose and reply in the web UI and the GUI | done (§7) |
| `serve`: the local web API, token-gated | done (§3, M5) |
| GUI (Tauri) and web (DOM), one page | done (§7, M5) |
| JSON contract: `--json`, `--ndjson`, `--fields`, cursors, exit codes | done (§6.4, §6.6) |
| `watch`: the mailbox as a cursored event stream | done (§6.4, M7) |
| `mcp`: the same operations as MCP tools over stdio | done (§6.4, M7) |
| `--trace`, `sync --once --until`, `debug store-dump`, `debug fixture` | done (§6.7) |
| Fixture format | done (§14.1) |
| L0 and L2 suites | done (§14.2) |

M1 through M5 and M7 are done; M6 (mobile) is below, and M8 — the cross-device
state folder, `mailbox devices`, and the pairing relay — is not started.

### The web interface

```sh
mailbox serve
#   http://127.0.0.1:38895/?mailbox_token=8678…
```

Loopback on an ephemeral port, behind a token, holding nothing across
restarts. `--bind 0.0.0.0` makes it reachable from the rest of the network and
prints a QR code so a phone can open it without anyone typing a token by hand;
that is refused without a token, since a stranger on the same Wi-Fi is exactly
who the token is for.

The token authenticates two things differently on purpose. The page gets a
cookie — `HttpOnly`, `SameSite=Strict` — so a reload works without the token in
the address bar. The API refuses that cookie and wants `X-Mailbox-Token`
instead, which another origin cannot send without a preflight, and preflights
are refused. That is what closes the cross-site request forgery a
cookie-authenticated local API otherwise has wide open.

`/api` answers with the §6.4 contract — the same JSON `--json` prints — and
there is deliberately no route that runs a command.

### The GUI and the web interface

`mailbox gui` opens a window; `mailbox serve` hands a browser a page. They are
the same DOM client — plain HTML, CSS and modules, no bundler and no framework,
which is the same reasoning as the hand-written HTTP it talks to — sitting on
the §6.4 contract. The window is Tauri (WebKitGTK on Linux, WKWebView on
macOS, WebView2 on Windows). Being a document rather than a canvas is what
gives it selection, find-in-page, the platform's own text rendering, and the
ability to show the sanitized HTML of §9 *as HTML*:

- the message renders in a sandboxed frame with `default-src 'none'` — no
  scripts, no same-origin, nothing fetched;
- remote images stay disarmed until you say otherwise, per sender and
  remembered, because loading them tells the sender you read it;
- links open in a new tab and carry no referrer.

Writing works from both: `m` for a new message, `r` and `R` to reply, `F` to
forward, Ctrl-Enter to send. The draft a reply starts from is built by the core
— what it quotes and which addresses a reply-all keeps have one right answer
per message, and two interfaces deriving them separately is how "reply" starts
meaning different things in different windows. Sending *queues* (§5.5): the
outbox is flushed by a sync pass, which is what makes the undo window and a
crash between "queued" and "sent" survivable.

The keys are the terminal's where they can be: `j`/`k`, `/`, `u`, `f`, `s`,
`d`, `z`. Below 45rem it folds to one pane at a time, which is what the QR code
`serve` prints is for.

Every interface follows the change feed, so a message deleted in the browser
disappears from the terminal within a second, and a flag set in one window
shows up in the other. A row that goes stale anyway — something expunged the
message between the last refresh and your keystroke — says so and puts the
listing right instead of failing.

A message you read is marked read. In the web and graphical interfaces the
reading pane fills in as the cursor moves, so it takes a second and a half of
actually being shown — hold `j` down through a mailbox and nothing is marked.
The terminal has no such pane: opening a message there is already deliberate,
so it marks straight away. Either way `-seen` puts it back.

### Moving to another device

`mailbox pair` on the new device shows a QR and a link; `mailbox pair send
<link>` on the old one sends the accounts, credentials, and identities across
the LAN. Both devices display the same four words, and nothing is written until
you confirm they match — that confirmation is what defends against someone
between the two machines (§8.1).

```
  █▀▀▀▀▀█ █  ▄▄ ▀█▄▀▀ █▀▄▄▄ ██  █▀▀▀▀▀█
  █ ███ █ ██▄ █▄▀▄▄█▄▀▀▀█▄▀▀ ▄  █ ███ █
  …

  mailbox pair send mailbox://pair/192.168.68.78:37485#544d…
```

Pairing is additive; `--replace` wipes this device's accounts first and needs
`--yes`. For an air-gapped move, `--export-file` writes the same payload sealed
under a passphrase and `--import-file` reads it back. **The mail store never
moves** — the new device syncs that from IMAP.

The relay and the short code are M8: a 45-bit code guarded by an unreviewed
PAKE is a credential leak rather than a convenience (§16).

### The terminal interface

```sh
mailbox tui
```

mutt-like muscle memory (§7): `j`/`k` to move, `/` to search, `t` tag, `d`
delete, `s` archive, `m` compose, `r`/`R` reply, `z` snooze, `o` to follow a
link, `|` pipe to a command, `?` for the keys. Vim-ish by default; `keymap = "emacs"` under `[keys]` swaps the
movement, and any action can be rebound:

```toml
[keys]
keymap  = "emacs"
archive = "e"
```

A rebinding *replaces* the old key rather than adding to it, and a name that
isn't an action is reported rather than ignored — a typo that silently does
nothing is a rebinding you believe is live.

Bodies are drawn as styled Markdown, which is what the core makes of HTML mail
— headings, emphasis, lists and quotes become the terminal's own styles rather
than a screenful of `**` and raw URLs. Links show their label and the number
`o` will offer:

```
Hi Peter, your parcel is on its way. You can track it here [2], or view
the order details [3].

• 1 × Kettle — €39.00
```

Plain-text mail comes through the same path and is left alone: a marker is only
a marker when its other half is on the same line, and emphasis may not begin or
end against a space, so `2 * 3 * 4`, `snake_case_names` and indented code all
survive intact.

Links are terminal hyperlinks (OSC 8) where the terminal supports them, so
right-click offers "copy link" and ctrl-click follows it;
`MAILBOX_HYPERLINKS=0` turns that off. `o` opens the picker, which shows whole
URLs — never a shortened form, since a link in mail is exactly the case where
the text and the destination disagree on purpose — with Enter to open one and
`y` to copy it. Copying goes through the terminal (OSC 52) rather than a
`wl-copy`/`pbcopy` helper, so it works over ssh and adds no display-server
dependency to the binary.

Everything is incremental (§7): the list paints from the store immediately, a
sync runs on a worker thread and is a word in the status bar rather than a modal
wait, and `/` filters as you type — prefix-matching, so `postm` finds
`postmortem`. The state machine holds no terminal types at all, so its behaviour
is tested by pressing actions rather than bytes.

### Notifications

New mail raises a desktop notification after each sync, filtered by a query
(§11):

```toml
[notify]
enabled      = true
query        = "is:unread in:inbox NOT tag:muted"
max_per_sync = 5
# command    = "/usr/local/bin/my-notifier"   # optional override
```

`mailbox notify --test` checks the platform mechanism works — it needs no store,
so it can be run before the first sync. `mailbox notify` shows what is waiting
without raising or consuming it.

Delivery runs the platform's own command (`notify-send`, `osascript`) rather
than binding to D-Bus or Cocoa — the same choice as the keychain, and it keeps
a platform dependency out of the static binary. A first sync announces nothing,
a backlog collapses to one "43 new messages", and agent mode raises nothing at
all.

### Offline and attachments

Bodies are fetched lazily (§5.1): headers and structure for everything, bodies
for the last 30 days plus whatever you open. `offline` on the account picks the
policy — `none` fetches no bodies ahead of time, `recent` is the default window,
`full` downloads everything. `show --offline` and `attach --offline` show what is
cached and go nowhere.

```sh
mailbox attach ^                 # what is in there
mailbox attach ^ 2 > invoice.pdf # one part, to stdout
mailbox attach ^ --all --extract --out ~/Downloads
```

Extraction treats the filename as hostile, because it is (§9): paths and
traversals are stripped, extensions a desktop would execute get a `.bin` suffix,
nothing is overwritten, and files are written `0600` with the executable bit
clear.

Everything M2 names is built: flags, moves, SMTP and the outbox, compose,
multi-account, and the unified inbox — with L3 on both sides, the double-send
test, and the crash-only oracle.

One verification item is outstanding: **L2 has not been widened to Cyrus and
Stalwart**, and the existing Dovecot suite is currently skipping, because the
Docker daemon on this machine is not reachable from this user. Everything else
runs without it.

### In progress: M6 (iOS)

The iOS app is a thin SwiftUI shell over `mailbox-ffi` — the same core, no web
view (§3). Inbox, thread, compose, search, and account setup are in
[`ios/`](ios/README.md). Android is not started. The OAuth2 flow is in the
core, so the shell needs a screen for it rather than an implementation; the
optional push notifier of §11 is not in this cut.

### Not built yet

- **The §4.1 owner socket.** The advisory lock is built — exactly one process
  is an account's sync engine, and a loser is told so truthfully, since its
  local writes are already queued and the owner pushes them. What is missing is
  the socket that would let the loser have its writes performed *now*.
- **Autoconfig discovery.** `accounts add` uses a built-in table for the big
  providers; §9 also wants `autoconfig`/`autodiscover`, DNS SRV, and the
  Mozilla ISPDB, so that an unknown domain does not need `--imap-host`.
- **`COMPRESS=DEFLATE`** and `raw:` server-side search passthrough.
- **The cross-device state folder** (§5.4), and with it shared tags, saved
  searches, and snooze that follows you between machines. All three are local
  today. M8.

## Building

```sh
cargo build --release      # target/release/mailbox
```

No runtime, no daemon, no database server. SQLite is compiled in.

`mailbox gui` uses the platform webview rather than vendoring one: WebKitGTK
4.1 on Linux (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`), WKWebView on macOS,
WebView2 on Windows. Those are the same class of dependency as the keychain —
a system library, not a second copy of a browser in the binary. Without a
display, `mailbox serve` is the same interface in a browser.

A build carries whatever OAuth2 client credentials are in the environment when
it is compiled, and none if there are none:

```sh
MAILBOX_GOOGLE_CLIENT_ID=… MAILBOX_GOOGLE_CLIENT_SECRET=… \
MAILBOX_MICROSOFT_CLIENT_ID=… MAILBOX_YAHOO_CLIENT_ID=… MAILBOX_YAHOO_CLIENT_SECRET=… \
    cargo build --release
```

Packagers should use their own rather than inherit this project's quota and
verification status; see [Adding an account](#oauth2). The test suite passes
either way, and asserts that the binary's behaviour matches what it was built
with.

### A mailbox with twenty years in it

The first sync does not fetch twenty years before showing you anything. Each
pass takes the newest few thousand messages a folder is still missing and walks
backwards, so the inbox is readable after the first one and a dropped
connection costs a chunk rather than the lot.

While that is going on the store holds *part* of a mailbox, and everything says
so — because a search over a partial mailbox that does not announce itself is a
wrong answer with no way to tell:

```
$ mailbox ls
     id       date         from            subject
●    ffe8c3f  Sep 01       Alice           Message 3138
…
still fetching older mail — about 3000 more, back to 2024-03-11; run `mailbox sync` again

$ mailbox status
work (peter@example.com)
  2000 messages, 2000 unread, 1 folders
  still fetching older mail: about 3000 messages to go — synced back to 2024-03-11
    INBOX: 2000 of 5000
```

`ls --json` and `search --json` carry `"partial": true` for the same reason,
and `status --json` carries the detail under `coverage`. Both stop saying
anything once the walk reaches the bottom. `sync --watch` finishes the backfill
on its own; otherwise each `mailbox sync` advances it.

## Adding an account

```sh
mailbox accounts add peter@gmail.com
```

For the providers in the built-in table — Gmail, Fastmail, Outlook, Yahoo,
iCloud, Proton — that is the whole of it; anywhere else needs at least
`--imap-host`. Full autoconfig discovery is still to come, so an unknown domain
asks rather than guesses. `accounts edit <id>` changes a setting, `accounts
remove <id>` takes the account out of the config and leaves the mail cached on
disk (it names the directory so you can delete it deliberately), and `accounts
test <id>` connects and reports what the server offers.

The password never reaches the config file (§9). `--credential-command 'pass
show mail/work'` is the durable way to supply one; `--password-stdin` reads it
from a pipe for a one-off.

```sh
mailbox accounts add peter@example.org \
    --imap-host imap.example.org \
    --credential-command 'pass show mail/work'
```

### OAuth2

```sh
mailbox accounts add peter@gmail.com --oauth google --login
mailbox accounts login peter          # again, whenever the grant is revoked
```

The system browser opens, the redirect comes back to a loopback port bound
before the browser is launched, and PKCE (S256) is what stops anyone who sees
the authorization code from using it. Google, Microsoft, and Yahoo are known;
`oauth_auth_url`, `oauth_token_url`, and `oauth_scope` point at anything else.
On a machine with no browser, `--no-browser` prints the URL and waits.

#### Whose registration

A release build carries mailbox's own OAuth2 client, so the command above is
the whole of it. `--oauth-client-id` overrides it per account, which is what
you want if you would rather spend your own quota, show your own consent
screen, or not be subject to this project's verification status:

```sh
mailbox accounts add peter@gmail.com --oauth google \
    --oauth-client-id 1234.apps.googleusercontent.com
```

The client *secret* Google and Yahoo ask for is not one. RFC 8252 is explicit
that an installed application cannot hold a confidential secret — it ships in
every copy of the binary — so it is an identifier that happens to be
awkwardly named, and PKCE is what actually protects the flow. Set it with
`oauth_client_secret` in the account's stanza, or with
`MAILBOX_OAUTH_CLIENT_SECRET_<ACCOUNT>` to keep it out of the file. Microsoft
needs no secret at all: Azure public clients authenticate without one.

**Packaging.** The built-in credentials come from the environment at build
time, not from the source tree:

```sh
MAILBOX_GOOGLE_CLIENT_ID=… MAILBOX_GOOGLE_CLIENT_SECRET=… \
MAILBOX_MICROSOFT_CLIENT_ID=… \
    cargo build --release
```

A build with none of them set ships none, and `--oauth google` then asks for
`--oauth-client-id` and links to the right console — which is the right
behaviour for a distribution that would rather register its own application
than put every user of the package under someone else's quota. `mailbox
accounts login --json` reports `"client": "built-in"` or `"configured"`, so it
is always answerable whose registration an account is using. See §16.

The refresh token goes to the OS keychain where there is one, and otherwise to
`oauth.json` in the account's state directory, mode 0600 inside a directory
that is 0700 — the same protection §9 gives the mail itself. The access token
is cached beside it and renewed automatically when it expires, so no command
waits on a browser. `mailbox pair` carries the refresh token, so a second
device does not repeat the flow.

### By hand

An account is a stanza in `$XDG_CONFIG_HOME/mailbox/config.toml` (default
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

`max_per_hour` counts mail this account is *committed* to sending — queued and
awaiting approval as well as delivered — because the thing it guards against is
a loop, and a loop that queues two hundred messages in a second has delivered
none of them yet. Cancelling gives the slot back; retrying after a failed
delivery does not spend a second one.

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
and the test suite validates real output against it — for every subcommand the
binary offers, since §14.4 makes that gating: a new command must either publish
a schema or be recorded as emitting no document, and the test reads the command
list out of `--help` so neither can be forgotten.

```sh
MAILBOX_AGENT=1 mailbox ls --json --fields id,subject,from
```

Under `MAILBOX_AGENT=1`, any state-changing command needs an explicit `--yes`.

`mailbox watch` is the loop: new mail, flag changes, moves, and sends, one
NDJSON event per line as they happen.

```sh
mailbox --ndjson watch --events new
{"schema":"mailbox/v1","command":"watch","payload":{"event":"started","cursor":"work:184","syncing":true}}
{"schema":"mailbox/v1","command":"watch","payload":{"seq":185,"event":"new","account":"work",
 "message":"9c1f0a3…","folder":"INBOX","local":false,"detail":{"subject":"Invoice 42","from":"alice@example.com"}}}
```

Every event carries a `seq`, and the `cursor` it adds up to resumes the stream:
`watch --drain --cursor <cursor>` is "what happened while I was away", which is
what makes a consumer that gets restarted — or crashes halfway through acting
on an event — miss nothing and repeat rather than skip. If the feed was trimmed
past that cursor, `truncated` says so instead of pretending the gap is not
there. `local` distinguishes this device's own writes from everyone else's, so
an agent that flags what it reads does not wake itself up.

Events are recorded in the same transaction as the change they describe, so
there is no state in which one exists without the other. With nothing else
running, `watch` becomes the sync engine itself (§4.1) so that mail actually
arrives; beside a TUI or a `sync --watch` it follows instead, and the `started`
event says which.

`mailbox mcp` serves the same operations as MCP tools over stdio, for a model
that speaks that instead of a shell:

```json
{ "mcpServers": { "mailbox": { "command": "mailbox", "args": ["mcp"] } } }
```

It is read-only as launched: the tools that change anything — flag, move,
send, sync — are not merely refused but **not listed**, because a model cannot
be talked into calling a tool it was never shown. `mailbox mcp --yes` offers
them. Sending still goes through the outbox, so `policy.toml`, the approval
queue, and idempotency keys apply exactly as they do to `mailbox send`: an
unattended `send_message` comes back `pending_approval` and waits for a person.
There is no second send path here to get that wrong.

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

`MAILBOX_NOTIFIER=none` silences desktop notifications, and any other value is
a command to run instead of the platform's (`{summary}` and `{body}` as
arguments) — which is how the suite tests §11 without putting popups on the
desktop of whoever ran it. Every harness sets it.

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

### UI evidence, in a VM

TUI, web, and GUI end-to-end live in `tests/e2e/`: a libvirt guest, mail only
on loopback, screenshots and video as evidence. Not a PR gate.

```sh
cargo build -p mailbox-cli
tests/e2e/mailbox-e2e create && tests/e2e/mailbox-e2e provision
tests/e2e/mailbox-e2e stop && tests/e2e/mailbox-e2e isolate
tests/e2e/mailbox-e2e start && tests/e2e/mailbox-e2e run all
```

See [tests/e2e/README.md](tests/e2e/README.md).

Nothing in the suite depends on wall-clock time or on a server-assigned value:
`MAILBOX_CLOCK` and `MAILBOX_SEED` pin the clock and the RNG (§6.7), and
fixtures name messages symbolically (§14.1).

## Layout

```
crates/mailbox-api    the JSON contract — types only, no logic
crates/mailbox-core   store, sync, query, threading, MIME
crates/mailbox-cli    the `mailbox` binary
crates/mailbox-tui    the terminal interface
crates/mailbox-ui     the GUI (Tauri, around the web client)
crates/mailbox-web    the local web UI, and the page the GUI shows
crates/mailbox-ffi    C ABI for the native mobile shells
ios/                  SwiftUI app over mailbox-ffi
tests/dovecot         the L2 server, configured in-tree
tests/e2e             KVM UI evidence: TUI, web, GUI, isolated mail
```

If a frontend needs logic that isn't in `mailbox-core`, that logic belongs in
`mailbox-core` (§4).

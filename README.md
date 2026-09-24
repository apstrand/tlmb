# The Last Mailbox

The Last Mailbox (`mailbox`) is an email app for existing IMAP and SMTP accounts.
Read, search, organize, and send mail from a desktop window, a browser, or the
terminal. It supports multiple accounts, a unified inbox, conversation threads,
attachments, and offline access to cached mail.

See the [short user guide](docs/USER-GUIDE.md) for the graphical interface and
the [technical guide](docs/TECHNICAL-GUIDE.md) for architecture, mail flows,
tradeoffs, and the security model.

## Build

You need Rust 1.90 or newer, Cargo, and a C/C++ build toolchain. The desktop
interface uses the system webview: WebKitGTK 4.1 on Linux, WKWebView on macOS,
and WebView2 on Windows. On Debian or Ubuntu, install the Linux dependencies:

```sh
sudo apt install build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev
```

From the repository root:

```sh
cargo build --release -p mailbox-cli --locked
./target/release/mailbox --help
```

The executable is `target/release/mailbox` (`mailbox.exe` on Windows). Add its
directory to your `PATH` to use the commands below. SQLite is included; no
separate database server is needed.

Builds stamp in the commit they came from, which `mailbox --version` prints and
the status bar shows. That comes from `git` in the source tree; a build from a
tarball has none, so a packager can supply it instead through
`MAILBOX_BUILD_COMMIT`, `MAILBOX_BUILD_COMMIT_DATE` and `MAILBOX_BUILD_TAG`.
Set `MAILBOX_BUILD_RELEASE=1` to mark a build as an official release — the
packaging scripts below do this for you.

For a Linux Flatpak build:

```sh
flatpak install flathub org.flatpak.Builder
packaging/flatpak/build.sh --run
```

For a macOS app bundle:

```sh
packaging/macos/build.sh --run
```

That builds a universal `Mailbox.app` — Apple silicon and Intel — and copies
it to `/Applications`. It is unsigned unless `APPLE_SIGNING_IDENTITY` names a
Developer ID in your keychain, which is fine for a build you run yourself and
not enough for one you hand to someone else; `packaging/macos/build.sh --help`
lists what signing and notarizing a release needs. The binary inside the
bundle is the whole `mailbox` command, so the terminal commands below are a
symlink away:

```sh
ln -s /Applications/Mailbox.app/Contents/MacOS/mailbox /usr/local/bin/mailbox
```

For the native iOS app, see [ios/README.md](ios/README.md).

## Add an account

The app can discover mail server settings from your email address. In a
terminal, adding a password-based account prompts for its password and masks
the input with `*`:

```sh
mailbox accounts add you@example.org
```

The password is saved in the OS keychain when one is available. To replace it
later, use the same masked prompt:

```sh
mailbox accounts edit you --password
```

To use a password stored in a password manager instead, provide a command that
prints it:

```sh
mailbox accounts add you@example.org \
  --credential-command 'pass show mail/personal'
mailbox accounts test
mailbox sync
```

Here, `pass` is an example; use the command for your password manager. If your
provider requires an app password, use that password. For scripts,
`--password-stdin` reads a password from standard input and saves it in the OS
keychain when available. Without a supported keychain, it lasts only for that
command; use `--credential-command` for subsequent runs.

If discovery does not find your servers, supply them explicitly:

```sh
mailbox accounts add you@example.org \
  --imap-host imap.example.org --smtp-host smtp.example.org \
  --credential-command 'pass show mail/personal'
```

Google, Microsoft, and Yahoo accounts also support browser sign-in:

```sh
mailbox accounts add you@gmail.com --oauth google --login
```

Use `--oauth microsoft` or `--oauth yahoo` for those providers. Builds from
source do not include OAuth client credentials by default. Supply your own
registration with `--oauth-client-id`; Google and Yahoo also need a client
secret, configured with `MAILBOX_OAUTH_CLIENT_SECRET_<ACCOUNT>` (replace
`<ACCOUNT>` with the account ID). Packagers can embed credentials at build time
with `MAILBOX_GOOGLE_CLIENT_ID`, `MAILBOX_GOOGLE_CLIENT_SECRET`,
`MAILBOX_MICROSOFT_CLIENT_ID`, `MAILBOX_YAHOO_CLIENT_ID`, and
`MAILBOX_YAHOO_CLIENT_SECRET`.

Run `mailbox accounts` to see account IDs. Add more accounts the same way, and
use `--account <id>` to select one for a command. IDs may be email addresses.
When two accounts have the same local part, the first uses the short ID and the
next automatically uses its complete email address.

## Run

```sh
mailbox gui       # Desktop window
mailbox serve     # Browser interface: open the URL it prints
mailbox tui       # Terminal interface: press ? for keyboard shortcuts
```

The browser interface listens on localhost and prints an authenticated URL.
Keep the command running while using it.

You can also work directly from the command line:

```sh
mailbox ls
mailbox show 1
mailbox search 'from:alice is:unread'
mailbox compose
mailbox sync
mailbox status
```

List an account's folders and the roles reported by its server with
`mailbox folders --account work --remote`. A server that supports writable
IMAP SPECIAL-USE metadata can change an existing folder's role:

```sh
mailbox folders use "Saved Mail" archive --account work
mailbox folders use "Old Trash" none --account work
```

The command changes the server first, then refreshes every folder role in the
local store. Valid roles are `all`, `archive`, `drafts`, `flagged`, `important`,
`junk`, `sent`, and `trash`; `none` clears a role.

`SPECIAL-USE` advertisement alone makes roles readable, not writable. Dovecot
usually defines them in its server-side namespace configuration, for example
`mailbox Trash { special_use = \\Trash }`. Change that configuration and reload
Dovecot, then run `mailbox sync`; an ordinary IMAP login cannot override it.
`folders use` works only when the server makes `/private/specialuse` writable;
advertising IMAP `METADATA` does not guarantee that particular attribute can
be changed.

If the server marks several folders with the same role, choose which one the
client uses for commands such as `rm`, `archive`, and `spam`:

```sh
mailbox folders prefer trash "Trash" --account work
```

This preference is local to the account and survives sync. Use
`mailbox folders prefer trash none --account work` to return to the default
ordering.

`show 1` opens the first message from the last listing. `compose` opens your
`$EDITOR` and queues the message; `sync` sends queued mail and updates the local
store. Use `mailbox sync --watch` for continuous background sync, or
`mailbox sync --full` to download all message bodies for offline use.

Run `mailbox --help` or `mailbox <command> --help` for more options. Commands
also support `--json` for scripts and integrations.

## How it works

One Rust core handles mail across the interfaces. It syncs with your provider
over IMAP, keeps a local SQLite index and cached message files, and sends through
SMTP. Search and threading run locally. Changes and outgoing messages are queued
on disk and applied during sync. Bodies are fetched as needed, with recent mail
cached ahead of time; offline access covers what has been downloaded.

Configuration defaults to `~/.config/mailbox/config.toml`, and local mail data
to `~/.local/share/mailbox/` (respecting `XDG_CONFIG_HOME` and `XDG_DATA_HOME`).
Passwords are resolved through a keychain or credential command rather than
stored in the configuration file. HTML mail is sanitized, and remote images
stay blocked until you allow them.

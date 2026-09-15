# The Last Mailbox

The Last Mailbox (`mailbox`) is an email app for existing IMAP and SMTP accounts.
Read, search, organize, and send mail from a desktop window, a browser, or the
terminal. It supports multiple accounts, a unified inbox, conversation threads,
attachments, and offline access to cached mail.

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

For a Linux Flatpak build:

```sh
flatpak install flathub org.flatpak.Builder
packaging/flatpak/build.sh --run
```

For the native iOS app, see [ios/README.md](ios/README.md).

## Add an account

The app can discover mail server settings from your email address. To use a
password stored in a password manager, provide a command that prints it:

```sh
mailbox accounts add you@example.org \
  --credential-command 'pass show mail/personal'
mailbox accounts test
mailbox sync
```

Here, `pass` is an example; use the command for your password manager. If your
provider requires an app password, use that password. Alternatively,
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
use `--account <id>` to select one for a command.

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

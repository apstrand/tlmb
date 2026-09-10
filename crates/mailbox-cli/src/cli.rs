//! Argument definitions (§6.1).
//!
//! The CLI is the reference interface: the TUI and GUI are conveniences over
//! it, and its `--json` output *is* the API contract (§4). So the flags here
//! are not a convenience layer over a library — they are the API's surface, and
//! adding one is the same kind of commitment as adding a JSON field.

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "mailbox",
    version,
    about = "A self-contained IMAP & SMTP client",
    // Human help stays short; the JSON contract is the documentation agents
    // read, via `mailbox schema`.
    max_term_width = 100
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalFlags,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args, Clone)]
pub struct GlobalFlags {
    /// Emit exactly one JSON document (§6.4).
    #[arg(long, global = true)]
    pub json: bool,

    /// Emit one JSON object per line, for streaming lists.
    #[arg(long, global = true, conflicts_with = "json")]
    pub ndjson: bool,

    /// Restrict JSON output to these fields, comma-separated.
    #[arg(long, global = true, value_delimiter = ',')]
    pub fields: Option<Vec<String>>,

    /// Which account to act on. Defaults to the only one configured.
    #[arg(long, short = 'a', global = true)]
    pub account: Option<String>,

    /// Maximum rows to return.
    #[arg(long, short = 'n', global = true)]
    pub limit: Option<usize>,

    /// Continue a previous page (§6.4). Opaque; comes from a prior `cursor`.
    #[arg(long, global = true)]
    pub cursor: Option<String>,

    /// Show what would happen without doing it.
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Answer yes to any confirmation.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Suppress non-essential output.
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    pub color: ColorChoice,

    /// Give up after this many milliseconds.
    #[arg(long, global = true)]
    pub timeout: Option<u64>,

    /// Use this config file instead of the default.
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// `agent` makes state-changing commands require `--yes` (§6.4).
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// Write an NDJSON trace of every IMAP exchange and store transaction
    /// (§6.7). Defaults to `mailbox.trace.ndjson`.
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "mailbox.trace.ndjson")]
    pub trace: Option<std::path::PathBuf>,

    /// Include message bodies in the trace. Off by default so a trace can be
    /// attached to a bug report.
    #[arg(long, global = true, requires = "trace")]
    pub trace_bodies: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Plain text, HTML flattened when there is no text part.
    Text,
    /// HTML converted to Markdown, for LLM consumers (§6.4).
    Md,
    /// Sanitized HTML.
    Html,
    /// Exact RFC5322 bytes (§13).
    Raw,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List messages (default: the inbox, unread first).
    Ls(LsArgs),

    /// Render a message or a thread.
    Show(ShowArgs),

    /// Full-text and structured search (§6.2).
    Search(SearchArgs),

    /// Add or remove flags and tags: `+seen`, `-flagged`, `+tag:invoices`.
    Flag(FlagArgs),

    /// Send a message non-interactively (flags, or RFC5322/JSON on stdin).
    Send(SendArgs),

    /// Open $EDITOR on a new message, then queue it.
    Compose(ComposeArgs),

    /// Reply to, reply-all to, or forward a message.
    Reply(ReplyArgs),

    /// Inspect and manage queued outgoing mail.
    Outbox(OutboxArgs),

    /// List or extract a message's attachments.
    Attach(AttachArgs),

    /// Set a message aside until later (§7).
    Snooze(SnoozeArgs),

    /// Start the web interface (§3).
    Serve(ServeArgs),

    /// Launch the graphical interface (§7).
    Gui,

    /// Move messages to a folder.
    #[command(visible_alias = "move")]
    Mv(MoveArgs),

    /// Copy messages into a folder, leaving the originals.
    Cp(MoveArgs),

    /// Move messages to Trash. Sugar over `mv` (§6.1).
    Rm(TargetArgs),

    /// Move messages to Archive. Sugar over `mv` (§6.1).
    Archive(TargetArgs),

    /// Move messages to Junk. Sugar over `mv` (§6.1).
    Spam(TargetArgs),

    /// List folders with counts.
    Folders(FoldersArgs),

    /// List accounts, or test one.
    Accounts(AccountsArgs),

    /// Run a sync pass.
    Sync(SyncArgs),

    /// Stream mailbox events: new mail, flags, moves, sends (§6.4).
    Watch(WatchArgs),

    /// Serve the same operations as MCP tools over stdio (§6.4).
    Mcp(McpArgs),

    /// Queues, errors, connectivity, store size.
    Status,

    /// Inspect or exercise desktop notifications (§11).
    Notify(NotifyArgs),

    /// Launch the terminal interface (§7).
    Tui,

    /// Move accounts and credentials to another device (§8).
    Pair(PairArgs),

    /// Emit the JSON Schema for a command's output (§6.4).
    Schema(SchemaArgs),

    /// Introspection hooks (§6.7). Hidden: they exist for tests and bug
    /// reports, not for daily use.
    #[command(hide = true)]
    Debug(DebugArgs),
}

#[derive(Debug, Args)]
pub struct LsArgs {
    /// A query in the §6.2 grammar. Defaults to `in:inbox`.
    pub query: Option<String>,

    /// Group the results into threads.
    #[arg(long, short = 't')]
    pub threads: bool,

    /// Order by date rather than unread-first.
    #[arg(long)]
    pub by_date: bool,

    /// Oldest first.
    #[arg(long, conflicts_with = "by_date")]
    pub reverse: bool,

    /// Include the total match count. Costs a second pass over the match set.
    #[arg(long)]
    pub total: bool,

    /// Print full ids rather than the 7-character short form.
    #[arg(long)]
    pub full_ids: bool,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// A query in the §6.2 grammar.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,

    #[arg(long)]
    pub total: bool,

    #[arg(long)]
    pub full_ids: bool,
}

#[derive(Debug, Args)]
pub struct FlagArgs {
    /// Message ids and flag changes, in any order.
    ///
    /// Anything starting with `+` or `-` is a change; everything else is an id,
    /// an id prefix, `^`, `@`, or an index into the last listing (§5.2).
    ///
    /// Because `-flagged` is a value and not an option, global flags have to
    /// come before the subcommand: `mailbox --json flag 1 +seen`.
    #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address to listen on. Loopback by default (§3).
    ///
    /// `0.0.0.0` makes it reachable from the rest of the network, which is what
    /// the printed QR code is for — and is refused without a token.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: std::net::IpAddr,

    /// Port. Zero, the default, takes whatever is free.
    #[arg(long, short = 'p', default_value_t = 0)]
    pub port: u16,

    /// Use this token instead of a fresh one.
    ///
    /// For a bookmark that should keep working across restarts. It is as
    /// secret as a password, so it belongs in an environment variable or a
    /// keychain rather than in shell history.
    #[arg(long)]
    pub token: Option<String>,

    /// Serve without a token.
    ///
    /// Only for loopback, and only when you are the only user of the machine:
    /// every process running as any user could otherwise read your mail.
    #[arg(long)]
    pub no_token: bool,

    /// Print the URL and exit without serving.
    #[arg(long)]
    pub dry_run_url: bool,

    /// Stop after this many connections. For tests.
    #[arg(long, hide = true)]
    pub max_requests: Option<usize>,
}

#[derive(Debug, Args)]
pub struct SnoozeArgs {
    /// Messages, then when they should come back.
    ///
    /// The last argument is the time — `2h`, `tomorrow`, `+3d`, `2026-09-10` —
    /// and everything before it is a message: an id, a prefix, `^`, `@`, or an
    /// index into the last listing (§5.2). The same shape as `mv`, where the
    /// last argument is the destination (§6.1).
    ///
    /// Relative forms resolve against the clock at the moment you run this, so
    /// `2h` and `+2h` mean the same thing. Bare dates are UTC midnight, the
    /// same meaning `before:` and `after:` give them (§6.2).
    pub args: Vec<String>,

    /// Show what is asleep and when it comes back.
    #[arg(long, short = 'l')]
    pub list: bool,

    /// Wake the named messages now.
    #[arg(long)]
    pub clear: bool,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    #[arg(long, short = 't', value_delimiter = ',')]
    pub to: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub cc: Vec<String>,
    /// Blind copy. These reach the SMTP envelope and no header — which is the
    /// whole of what makes a blind copy blind.
    #[arg(long, value_delimiter = ',')]
    pub bcc: Vec<String>,
    #[arg(long, short = 's')]
    pub subject: Option<String>,
    /// The body. Read from stdin when absent.
    #[arg(long, short = 'b')]
    pub body: Option<String>,
    /// Which identity to send as. Defaults to the account's address.
    #[arg(long, short = 'f')]
    pub from: Option<String>,
    #[arg(long)]
    pub reply_to: Option<String>,
    /// Attach a file. Repeatable.
    #[arg(long)]
    pub attach: Vec<std::path::PathBuf>,

    /// Read a complete RFC5322 message from stdin instead of building one
    /// (§13, so `git send-email` and `sendmail`-style pipelines work).
    #[arg(long, conflicts_with_all = ["subject", "body"])]
    pub stdin: bool,

    /// Hold the message for this many seconds before sending (§5.5's undo
    /// window). Zero for the CLI unless given.
    #[arg(long)]
    pub delay: Option<u64>,

    /// §6.5: a retry with the same key returns the original result instead of
    /// sending twice.
    #[arg(long)]
    pub idempotency_key: Option<String>,

    /// Queue as a draft rather than for delivery.
    #[arg(long)]
    pub draft: bool,
}

#[derive(Debug, Args)]
pub struct ComposeArgs {
    #[arg(long, short = 't', value_delimiter = ',')]
    pub to: Vec<String>,
    #[arg(long, short = 's')]
    pub subject: Option<String>,
    #[arg(long, short = 'f')]
    pub from: Option<String>,
    #[arg(long)]
    pub attach: Vec<std::path::PathBuf>,
    #[arg(long)]
    pub delay: Option<u64>,
    /// Save as a draft rather than queueing it.
    #[arg(long)]
    pub draft: bool,
}

#[derive(Debug, Args)]
pub struct ReplyArgs {
    /// The message to reply to (§5.2's id forms).
    pub id: String,
    /// Reply to everyone, not just the sender.
    #[arg(long)]
    pub all: bool,
    /// Forward instead of replying.
    #[arg(long, conflicts_with = "all")]
    pub forward: bool,
    /// Recipients for a forward.
    #[arg(long, short = 't', value_delimiter = ',')]
    pub to: Vec<String>,
    /// The reply text. Opens $EDITOR when absent and stdin is a terminal.
    #[arg(long, short = 'b')]
    pub body: Option<String>,
    #[arg(long)]
    pub attach: Vec<std::path::PathBuf>,
    #[arg(long)]
    pub delay: Option<u64>,
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long)]
    pub draft: bool,
}

#[derive(Debug, Args)]
pub struct OutboxArgs {
    #[command(subcommand)]
    pub action: Option<OutboxAction>,
}

#[derive(Debug, Subcommand)]
pub enum OutboxAction {
    /// Show queued, failed, and recently sent mail.
    List {
        /// Only this state: draft, pending_approval, queued, sent, failed,
        /// cancelled.
        #[arg(long)]
        state: Option<String>,
    },
    /// Release a message held by the approval queue (§6.5).
    Approve { id: i64 },
    /// Try a failed or cancelled message again.
    Retry { id: i64 },
    /// Stop a message going out. Only works before it is sent.
    Cancel { id: i64 },
    /// Send everything that is ready, without a full sync.
    Flush,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    /// A message id, an unambiguous prefix, `^`, `@`, or a listing index.
    pub id: String,
    /// The MIME part to extract (`2`, `2.1`). Omit to list what is there.
    pub part: Option<String>,
    /// Write the part to disk. §9 clears the executable bit and mangles
    /// extensions a desktop would run.
    #[arg(long, short = 'x')]
    pub extract: bool,
    /// Where to write it. A directory, or a file path for a single part.
    #[arg(long, short = 'o', default_value = ".")]
    pub out: std::path::PathBuf,
    /// Extract every attachment, not just one part.
    #[arg(long, conflicts_with = "part")]
    pub all: bool,
    /// Never fetch from the server; use only what is cached.
    #[arg(long)]
    pub offline: bool,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    /// One or more message ids, then the destination folder.
    ///
    /// The folder is matched by full path, by leaf name, or by special-use — so
    /// `archive` finds whatever this server calls its archive.
    #[arg(required = true, num_args = 2..)]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct TargetArgs {
    #[arg(required = true, num_args = 1..)]
    pub ids: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// A message id, an unambiguous prefix, `^`, `@`, or an index into the
    /// last listing (§5.2).
    pub id: String,

    /// Show the whole thread.
    #[arg(long, short = 't')]
    pub thread: bool,

    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,

    /// Emit the exact RFC5322 bytes (§13). Implies `--format=raw`.
    #[arg(long)]
    pub raw: bool,

    /// Never fall back to HTML when there is no text part.
    #[arg(long)]
    pub text_only: bool,

    /// Never fetch from the server; show only what is cached.
    ///
    /// §5.1 fetches bodies lazily, so opening older mail normally goes and gets
    /// it. This is for when there is no network, or when a command must not
    /// make one.
    #[arg(long)]
    pub offline: bool,

    /// Drop quoted replies and signatures.
    #[arg(long)]
    pub strip_quotes: bool,

    /// Truncate the body, setting `"truncated": true`.
    #[arg(long)]
    pub max_body_bytes: Option<usize>,

    /// Omit the attachment list.
    #[arg(long)]
    pub no_attachments: bool,

    /// Print headers only.
    #[arg(long)]
    pub headers_only: bool,

    /// List the links in the body instead of the body.
    ///
    /// The numbering is the one the rendered text uses, so `[2]` in a message
    /// is the second line here.
    #[arg(long)]
    pub links: bool,
}

#[derive(Debug, Args)]
pub struct FoldersArgs {
    /// Ask the server instead of reading the local store.
    ///
    /// Shows what is up there and how big it is without syncing anything —
    /// nothing is fetched and nothing is written, so it works on an account
    /// that has never synced.
    #[arg(long, short = 'r')]
    pub remote: bool,
}

#[derive(Debug, Args)]
pub struct AccountsArgs {
    #[command(subcommand)]
    pub action: Option<AccountsAction>,
}

#[derive(Debug, Subcommand)]
pub enum AccountsAction {
    /// List configured accounts.
    List,
    /// Connect, authenticate, and report what the server offers.
    Test {
        /// Which account. Defaults to `--account` or the only one configured.
        account: Option<String>,
    },
    /// Add an account (§9).
    Add(AddAccountArgs),
    /// Change an existing account's settings.
    Edit(EditAccountArgs),
    /// Remove an account from the config. The local store is left alone.
    Remove {
        /// The account id, as `accounts list` prints it.
        id: String,
    },
    /// Sign in with OAuth2 through the system browser (§9).
    Login(LoginArgs),
    /// Look up a domain's mail servers, and change nothing (§9).
    ///
    /// What `add` does before it writes a config, printed instead: every
    /// source that was asked and what it said. For finding out why a domain
    /// needs `--imap-host`.
    Discover(DiscoverArgs),
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// The email address to look up.
    pub address: String,

    /// Do not connect to a host to confirm it speaks IMAP.
    ///
    /// Faster, and the only thing that runs on a machine with no route to the
    /// mail server — but a guessed host is then reported as a guess and would
    /// not be written to a config.
    #[arg(long)]
    pub no_verify: bool,

    /// The built-in table only. No packet leaves the machine.
    #[arg(long)]
    pub offline: bool,
}

/// The settings `accounts add` and `accounts edit` share.
#[derive(Debug, Args, Clone, Default)]
pub struct AccountSettings {
    #[arg(long, value_name = "HOST")]
    pub imap_host: Option<String>,
    #[arg(long, value_name = "PORT")]
    pub imap_port: Option<u16>,
    /// `implicit` (993) | `starttls` (143) | `none`.
    #[arg(long, value_name = "MODE")]
    pub imap_security: Option<String>,
    /// The login name, when it is not the address.
    #[arg(long, value_name = "USER")]
    pub imap_user: Option<String>,

    #[arg(long, value_name = "HOST")]
    pub smtp_host: Option<String>,
    #[arg(long, value_name = "PORT")]
    pub smtp_port: Option<u16>,
    /// `starttls` (587) | `implicit` (465) | `none`.
    #[arg(long, value_name = "MODE")]
    pub smtp_security: Option<String>,
    #[arg(long, value_name = "USER")]
    pub smtp_user: Option<String>,

    /// §9's hook: a command that prints the password on stdout, such as
    /// `pass show mail/work`. The recommended way to hold a password.
    #[arg(long, value_name = "COMMAND")]
    pub credential_command: Option<String>,

    /// Read a password from stdin and store it in the OS keychain (§9).
    ///
    /// Never written to the config. Without a keychain to put it in, this
    /// installs it for the running process only — enough to finish a setup and
    /// test it — and says so.
    #[arg(long)]
    pub password_stdin: bool,

    /// Authenticate with OAuth2: `google`, `microsoft`, or `yahoo`.
    #[arg(long, value_name = "PROVIDER")]
    pub oauth: Option<String>,

    /// Use this OAuth2 client rather than the one this build ships (§9).
    ///
    /// Register an application with the provider if you would rather spend
    /// your own quota and show your own consent screen. Public by design for
    /// an installed app: PKCE, not secrecy, protects the flow.
    #[arg(long, value_name = "ID")]
    pub oauth_client_id: Option<String>,

    /// Allow plaintext. Honoured only for a loopback host (§9).
    #[arg(long)]
    pub insecure: bool,
}

#[derive(Debug, Args)]
pub struct AddAccountArgs {
    /// The email address. Everything else is discovered or defaulted.
    pub address: String,

    /// The config key, and what `--account` takes. Defaults to the local part
    /// of the address.
    #[arg(long)]
    pub id: Option<String>,

    #[command(flatten)]
    pub settings: AccountSettings,

    /// Run the OAuth2 login straight after adding the account.
    #[arg(long, requires = "oauth")]
    pub login: bool,

    /// Do not look the servers up on the network (§9).
    ///
    /// Leaves the built-in table as the only source, which is what an air-gapped
    /// setup and a scripted one want. Implied by `--imap-host`, which has
    /// already answered the question discovery would ask.
    #[arg(long)]
    pub no_discover: bool,
}

#[derive(Debug, Args)]
pub struct EditAccountArgs {
    /// The account id, as `accounts list` prints it.
    pub id: String,

    /// Change the address this account sends and receives as.
    #[arg(long)]
    pub address: Option<String>,

    #[command(flatten)]
    pub settings: AccountSettings,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Which account. Defaults to `--account` or the only one configured.
    pub account: Option<String>,

    /// Print the URL and wait, without opening a browser.
    ///
    /// For a machine with no browser: open the URL somewhere that has one, and
    /// forward this machine's redirect port to finish the flow.
    #[arg(long)]
    pub no_browser: bool,

    /// How long to wait for the browser, in seconds.
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    pub wait: u64,
}

/// `mailbox mcp` (§6.4): the contract as MCP tools.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// Offer the tools that change the mailbox: flag, move, send, sync.
    ///
    /// A synonym for `--yes`, which §6.4 already defines as the consent that
    /// unlocks a state-changing command in agent mode. Without it those tools
    /// are not listed at all — a model cannot be argued into calling a tool it
    /// has never been shown.
    #[arg(long)]
    pub write: bool,
}

/// `mailbox watch` (§6.4): the event stream an agent loops on.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// Only these events, comma-separated: new, flag, move, copy, tag, sent,
    /// deleted.
    #[arg(long, value_delimiter = ',')]
    pub events: Option<Vec<String>>,

    /// Replay events from this far back before streaming: `2h`, `7d`, an
    /// RFC3339 time. Without it, the stream starts here and now.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Exit after this many events. Without it, runs until interrupted.
    #[arg(long, value_name = "N")]
    pub max_events: Option<usize>,

    /// Stop after this long, in seconds.
    ///
    /// Separate from `--timeout`, which is how long a network operation may
    /// take: a stream that ended because nothing happened has not timed out.
    #[arg(long, value_name = "SECONDS")]
    pub stop_after: Option<f64>,

    /// Drain what is already there and exit, rather than waiting for more.
    ///
    /// The polling shape, for a caller that would rather own its own loop:
    /// `watch --drain --cursor <last>` is "what happened while I was away".
    #[arg(long)]
    pub drain: bool,

    /// Do not sync — follow a mailbox someone else is keeping up to date.
    ///
    /// Off by default: a lone `watch` becomes the sync engine (§4.1) so that
    /// new mail actually arrives. When another process already is one, this
    /// happens anyway, and the `started` event says which.
    #[arg(long)]
    pub no_sync: bool,

    /// How often to check the feed, in milliseconds. Local reads; the default
    /// is well under what a human notices.
    #[arg(long, default_value_t = 250, value_name = "MS")]
    pub interval: u64,

    /// The longest single `IDLE`, in seconds. As `sync --watch`.
    #[arg(long, default_value_t = 540, value_name = "SECONDS")]
    pub idle_window: u64,

    /// How often to sync when the server does not offer `IDLE`, in seconds.
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    pub poll: u64,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Run exactly one pass to quiescence and exit (§6.7).
    ///
    /// The default, and what every end-to-end test steps the engine with. The
    /// loop is `--watch`.
    #[arg(long, default_value_t = true)]
    pub once: bool,

    /// Keep syncing: wait on `IDLE` and sync again whenever the server speaks
    /// (§5.3).
    ///
    /// Runs until interrupted. Only one process may be an account's sync
    /// engine (§4.1), so this refuses to start if another already is.
    #[arg(long, conflicts_with_all = ["until", "since"])]
    pub watch: bool,

    /// How often to sync when the server does not offer `IDLE`, in seconds.
    #[arg(long, default_value_t = 300, value_name = "SECONDS")]
    pub poll: u64,

    /// The longest single `IDLE` before re-issuing it, in seconds.
    ///
    /// RFC 2177's ceiling is 29 minutes, and this is clamped to it. The default
    /// is well under it because a NAT that drops an idle connection does so
    /// silently. Anything under a minute reconnects more than it waits, and is
    /// only useful for testing.
    #[arg(long, default_value_t = 540, value_name = "SECONDS")]
    pub idle_window: u64,

    /// Stop after this many passes. For tests.
    #[arg(long, hide = true)]
    pub max_passes: Option<usize>,

    /// Fetch every body, for offline use.
    #[arg(long)]
    pub full: bool,

    /// Restrict to these folders.
    #[arg(long, value_delimiter = ',')]
    pub folder: Option<Vec<String>>,

    /// Only look at messages newer than this date (§6.2 date syntax).
    #[arg(long)]
    pub since: Option<String>,

    /// Stop at a named trace event, for mid-operation assertions (§6.7).
    #[arg(long, requires = "trace")]
    pub until: Option<String>,
}

#[derive(Debug, Args)]
pub struct PairArgs {
    #[command(subcommand)]
    pub action: Option<PairAction>,

    /// Write the payload to an encrypted file instead, for an air-gapped move.
    ///
    /// Mutually exclusive with `send` and `--import-file`; clap cannot express a
    /// conflict against a subcommand, so `pair` checks it and says which.
    #[arg(long)]
    pub export_file: Option<std::path::PathBuf>,

    /// Read a payload from an encrypted file.
    #[arg(long)]
    pub import_file: Option<std::path::PathBuf>,

    /// Add the incoming accounts to this device's. The default (§8.3).
    #[arg(long, conflicts_with = "replace")]
    pub merge: bool,

    /// Wipe this device's accounts first. Requires `--yes` (§8.3).
    #[arg(long)]
    pub replace: bool,

    /// The passphrase for an export or import. Prompted for when absent, or
    /// read from `MAILBOX_PAIR_PASSPHRASE`.
    #[arg(long)]
    pub passphrase: Option<String>,

    /// Print the pairing link as text only, without the QR block.
    #[arg(long)]
    pub no_qr: bool,
}

#[derive(Debug, Subcommand)]
pub enum PairAction {
    /// Send this device's accounts to one that is waiting.
    ///
    /// Takes the `mailbox://pair#…` link the other device is showing — scanned
    /// from its QR, or pasted.
    Send {
        link: String,
        /// Skip the confirmation prompt. §8.1's short authentication string is
        /// what defends against an active attacker, so this is for scripts that
        /// have already established trust some other way.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
pub struct NotifyArgs {
    /// Raise a sample notification, to check the platform mechanism works.
    #[arg(long)]
    pub test: bool,
    /// Show what would be announced without announcing it, and without marking
    /// anything as seen.
    #[arg(long)]
    pub pending: bool,
}

#[derive(Debug, Args)]
pub struct SchemaArgs {
    /// Which command's output schema. Omit to list the commands that have one.
    pub command: Option<String>,
}

#[derive(Debug, Args)]
pub struct DebugArgs {
    #[command(subcommand)]
    pub action: DebugAction,
}

#[derive(Debug, Subcommand)]
pub enum DebugAction {
    /// Canonical, ordered dump of derived state, for golden-file comparison.
    StoreDump {
        /// Restrict the message set. Folders and counts are always whole.
        query: Option<String>,
    },
    /// Print the clock the process is using, and where it came from.
    Clock,
    /// Seed a store from a fixture manifest (§14.1).
    Fixture {
        #[command(subcommand)]
        action: FixtureAction,
    },
    /// Parse a query and print the AST, for grammar tests.
    Query { query: String },
    /// Rebuild every thread from scratch.
    Rethread,
    /// Fill the store with a generated corpus (§14.1, §14.5).
    ///
    /// Deterministic from `--seed`, so two machines measure the same mailbox.
    Generate {
        #[arg(long, default_value_t = 1_000_000)]
        messages: u64,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 8)]
        folders: u32,
    },

    /// Measure the §10 budgets against the current store (§14.5).
    Bench {
        /// Timed runs per operation. The median is reported.
        #[arg(long, default_value_t = 9)]
        samples: u32,
        /// Compare against a recorded baseline and fail on a regression.
        #[arg(long)]
        baseline: Option<std::path::PathBuf>,
        /// How much slower than the baseline is tolerated, as a fraction.
        #[arg(long, default_value_t = 0.10)]
        tolerance: f64,
        /// Write this run out as the new baseline.
        #[arg(long)]
        save_baseline: Option<std::path::PathBuf>,
        /// Also fail when a measurement exceeds its absolute §10 budget.
        ///
        /// Off by default: §14.5 gates on regression rather than on the
        /// absolute number, so the numbers stay honest instead of being retuned
        /// whenever they hurt.
        #[arg(long)]
        budgets: bool,
    },

    /// Replay a recorded IMAP conversation against the sync engine (§14.2, L1).
    ///
    /// No network, no server: the engine under test is the same code that talks
    /// to a real one.
    Replay {
        transcript: std::path::PathBuf,
        /// Fail if the client's commands differ from the recording. On by
        /// default — a replay that only checked responses would let the engine
        /// send anything at all and still pass.
        #[arg(long, default_value_t = true)]
        strict: bool,
        /// Refuse a recording older than this many days. §14.2: a stale
        /// transcript is a test that passes about a server that no longer
        /// exists.
        #[arg(long)]
        max_age_days: Option<i64>,
    },
}

#[derive(Debug, Subcommand)]
pub enum FixtureAction {
    /// Seed the local store directly, with no server in the loop.
    Load { path: std::path::PathBuf },
    /// Render the manifest to RFC5322 bytes, for seeding a server.
    Render {
        path: std::path::PathBuf,
        /// One message by name. Omit for all of them.
        #[arg(long)]
        name: Option<String>,
    },
}

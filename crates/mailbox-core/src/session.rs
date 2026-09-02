//! The session facade — the operations the contract names, over the store and
//! the sync engine.
//!
//! §4's rule is that frontends are thin: if a frontend needs logic that isn't
//! in the core, that logic belongs in the core. So the CLI parses arguments and
//! formats output, and *everything* between those two things is here. When the
//! TUI arrives in M3 it calls these same functions.

use std::path::PathBuf;

use mailbox_api::{
    Account, AccountList, AccountStatus, Folder, FolderList, MessageList, ShowResult, Status,
    SyncResult,
};

use crate::clock::{format_rfc3339, Clock};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::imap::sync::{sync_once, SyncOptions};
use crate::imap::{Client, ConnectionConfig};
use crate::query::{self, Node};
use crate::render::RenderOptions;
use crate::store::read::{ListOptions, Listing};
use crate::store::{Store, Written};
use crate::trace::Tracer;

/// How a session was asked to open.
#[derive(Debug, Clone, Default)]
pub struct SessionOptions {
    /// `--account`. Defaults per [`Config::default_account`].
    pub account: Option<String>,
    /// `--config`.
    pub config_path: Option<PathBuf>,
    /// Read-only skips ownership entirely (§4.1) — the fast path for scripts
    /// and agents.
    pub read_only: bool,
    /// `MAILBOX_AGENT=1` or `--profile agent` (§6.4).
    pub agent_mode: bool,
    /// `--timeout`, in milliseconds.
    pub timeout_ms: Option<u64>,
}

pub struct Session {
    pub config: Config,
    pub account_id: String,
    pub clock: Clock,
    pub tracer: Tracer,
    pub agent_mode: bool,
    timeout_ms: u64,
    store: Store,
}

/// How a message was named on the command line (§5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdRef {
    /// A full id or an unambiguous prefix.
    Id(String),
    /// `@` — whatever the TUI/GUI has selected.
    Selected,
    /// `^` — the most recent message printed by the previous command.
    Previous,
    /// `1`, `20` — an index into the last listing. Humans only.
    Index(usize),
}

impl IdRef {
    pub fn parse(s: &str) -> IdRef {
        match s {
            "@" => IdRef::Selected,
            "^" => IdRef::Previous,
            _ => match s.parse::<usize>() {
                Ok(n) if n > 0 => IdRef::Index(n),
                _ => IdRef::Id(s.to_string()),
            },
        }
    }
}

impl Session {
    pub fn open(opts: &SessionOptions, tracer: Tracer, clock: Clock) -> Result<Session> {
        let config = match &opts.config_path {
            Some(p) => Config::load(p)?,
            None => Config::load_default()?,
        };
        let account_id = match &opts.account {
            Some(a) => {
                // Fail here rather than at the first store access, so
                // `--account typo` says so instead of reporting an empty
                // mailbox.
                config.account(a)?;
                a.clone()
            }
            None => config
                .default_account()
                .map_err(|_| Error::not_found("account", "none configured"))?
                .to_string(),
        };

        let dir = crate::paths::account_dir(&account_id);
        let store = if opts.read_only {
            Store::open_read_only(&dir, &account_id, tracer.clone(), clock.clone())?
        } else {
            Store::open(&dir, &account_id, tracer.clone(), clock.clone())?
        };

        Ok(Session {
            config,
            account_id,
            clock,
            tracer,
            agent_mode: opts.agent_mode,
            timeout_ms: opts.timeout_ms.unwrap_or(30_000),
            store,
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    pub fn account_config(&self) -> Result<&crate::config::AccountConfig> {
        self.config.account(&self.account_id)
    }

    /// Parses a query string against this session's clock.
    pub fn parse_query(&self, q: &str) -> Result<Node> {
        Ok(query::parse(q, &self.clock)?)
    }

    /// `ls` (§6.1): the unified inbox by default, unread first.
    ///
    /// "Unified inbox" is one account's inbox in M1 — the multi-account union
    /// arrives with M2, and the shape of this function is what it will extend.
    pub fn ls(&self, query_str: Option<&str>, mut opts: ListOptions) -> Result<MessageList> {
        let node = match query_str {
            Some(q) if !q.trim().is_empty() => self.parse_query(q)?,
            _ => self.parse_query("in:inbox")?,
        };
        opts.identities = self.identities();
        let listing = self.store.search(&node, &opts)?;
        Ok(self.to_list(listing))
    }

    /// `search` (§6.1): the same grammar, newest-first, over every folder.
    pub fn search(&self, query_str: &str, mut opts: ListOptions) -> Result<MessageList> {
        let node = self.parse_query(query_str)?;
        opts.identities = self.identities();
        let listing = self.store.search(&node, &opts)?;
        Ok(self.to_list(listing))
    }

    fn to_list(&self, listing: Listing) -> MessageList {
        MessageList {
            messages: listing.messages,
            cursor: listing.cursor,
            total: listing.total,
        }
    }

    /// `show` (§6.1). With `as_thread`, the whole conversation.
    pub fn show(
        &self,
        id_ref: &IdRef,
        render: &RenderOptions,
        as_thread: bool,
    ) -> Result<ShowResult> {
        let id = self.resolve(id_ref)?;
        if as_thread {
            let summary = self.store.summary(&id)?;
            let thread = self.store.thread(&summary.thread_id)?;
            return Ok(ShowResult {
                message: None,
                thread: Some(thread),
            });
        }
        Ok(ShowResult {
            message: Some(self.store.message(&id, render)?),
            thread: None,
        })
    }

    pub fn raw(&self, id_ref: &IdRef) -> Result<Vec<u8>> {
        self.store.raw_message(&self.resolve(id_ref)?)
    }

    pub fn folders(&self) -> Result<FolderList> {
        Ok(FolderList {
            folders: self.store.folders()?,
        })
    }

    /// What the *server* holds, with counts, without syncing (§6.1).
    ///
    /// Answers "what is up there, and how big is it" before committing to a
    /// sync. It writes nothing: no folder is created locally, no message is
    /// fetched, and the store is not touched at all — so it works on an account
    /// that has never synced.
    pub fn remote_folders(&self) -> Result<FolderList> {
        let mut client = self.connect()?;
        let listed = client.list_folders_with_counts();
        let _ = client.logout();

        let mut listed = listed?;
        // The same order the local listing uses — inbox first, then the other
        // special-use folders, then the rest by path. Servers return LIST in
        // whatever order they please, and a `--remote` listing that disagreed
        // with the local one about where the inbox goes would be a needless
        // difference between two views of the same mailbox.
        listed.sort_by(|a, b| {
            (a.record.rank(), &a.record.path).cmp(&(b.record.rank(), &b.record.path))
        });

        Ok(FolderList {
            folders: listed
                .into_iter()
                .map(|f| Folder {
                    account: self.account_id.clone(),
                    path: f.record.path,
                    name: f.record.name,
                    special_use: f.record.special_use,
                    // A server that would not give a count reports zero rather
                    // than a missing field: the contract's counts are not
                    // optional, and a folder that cannot hold mail genuinely
                    // holds none.
                    total: f.total.unwrap_or(0),
                    unread: f.unread.unwrap_or(0),
                    selectable: f.record.selectable,
                })
                .collect(),
        })
    }

    /// Opens an authenticated connection for this account.
    ///
    /// Shared by `sync`, `accounts test`, and `folders --remote`, so the TLS
    /// policy of §9 is decided in exactly one place.
    pub fn connect(&self) -> Result<Client> {
        let cfg = self.account_config()?.clone();
        let host = cfg.imap_host.clone().ok_or_else(|| {
            Error::usage(format!(
                "account `{}` has no imap_host; autoconfig discovery lands with `accounts add`",
                self.account_id
            ))
        })?;
        let credential = crate::credentials::resolve(&self.account_id, &cfg)?;

        let conn_cfg = ConnectionConfig {
            host: host.clone(),
            port: cfg.port(),
            security: cfg.security(),
            // §9: plaintext is refused unless the host is loopback *and* the
            // user opted in. Both halves are checked here, so a config that
            // says `require_tls = false` for a public host still gets TLS.
            require_tls: cfg.require_tls || !is_loopback(&host),
            timeout: std::time::Duration::from_millis(self.timeout_ms),
        };

        let mut client = Client::connect(&conn_cfg, self.tracer.clone())?;
        client.login(cfg.login_user(), &credential)?;
        Ok(client)
    }

    pub fn accounts(&self) -> Result<AccountList> {
        let mut out = Vec::new();
        for (id, cfg) in &self.config.accounts {
            let has_credential = crate::credentials::resolve(id, cfg).is_ok();
            let last_sync = if *id == self.account_id {
                let s = self.store.sync_state();
                (s.last_full_sync > 0).then(|| format_rfc3339(s.last_full_sync))
            } else {
                None
            };
            out.push(Account {
                id: id.clone(),
                address: cfg.address.clone(),
                login_user: cfg.login_user().to_string(),
                imap_host: cfg.imap_host.clone().unwrap_or_default(),
                imap_port: cfg.port(),
                has_credential,
                last_sync,
            });
        }
        Ok(AccountList { accounts: out })
    }

    pub fn status(&self) -> Result<Status> {
        let cfg = self.account_config()?;
        let state = self.store.sync_state();
        Ok(Status {
            accounts: vec![AccountStatus {
                account: self.account_id.clone(),
                address: cfg.address.clone(),
                messages: self.store.message_count()?,
                unread: self.store.unread_count()?,
                folders: self.store.folder_count()?,
                owner: !self.store.is_read_only(),
                queued_ops: self.store.queued_op_count()?,
                conflicts: self.conflicts()?,
                last_sync: (state.last_full_sync > 0).then(|| format_rfc3339(state.last_full_sync)),
                last_error: None,
            }],
            store_bytes: self.store.disk_usage(),
        })
    }

    /// `flag` (§6.1): adds and removes flags and tags on messages.
    ///
    /// Local and instant (§5.3). The server hears about it on the next sync,
    /// and if it refuses, the conflict shows up in `mailbox status`.
    pub fn flag(&mut self, refs: &[IdRef], change: &FlagSpec) -> Result<Vec<Written>> {
        let ids = self.resolve_all(refs)?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let mut written = self
                .store
                .set_flags(&id, &change.add_flags, &change.remove_flags)?;
            if !change.add_tags.is_empty() || !change.remove_tags.is_empty() {
                self.store
                    .set_tags(&id, &change.add_tags, &change.remove_tags)?;
                written.changed = true;
            }
            out.push(written);
        }
        Ok(out)
    }

    /// `mv` (§6.1). `cp`, `rm`, `archive`, and `spam` are sugar over this.
    pub fn move_to(&mut self, refs: &[IdRef], target: &str) -> Result<Vec<Written>> {
        let ids = self.resolve_all(refs)?;
        ids.into_iter()
            .map(|id| self.store.move_message(&id, target))
            .collect()
    }

    /// `cp` (§6.1): the message ends up in both folders.
    pub fn copy_to(&mut self, refs: &[IdRef], target: &str) -> Result<Vec<Written>> {
        let ids = self.resolve_all(refs)?;
        ids.into_iter()
            .map(|id| self.store.copy_message(&id, target))
            .collect()
    }

    /// The folder a special-use name resolves to on this account.
    ///
    /// `rm`, `archive`, and `spam` are moves to whatever the server calls
    /// Trash, Archive, and Junk — which is not the same string everywhere, and
    /// is not the user's problem.
    pub fn special_folder(&self, use_name: &str) -> Result<String> {
        self.store
            .folders()?
            .into_iter()
            .find(|f| f.special_use.as_deref() == Some(use_name))
            .map(|f| f.path)
            .ok_or_else(|| {
                Error::not_found("folder", format!("{use_name} (this account has none)"))
            })
    }

    fn resolve_all(&self, refs: &[IdRef]) -> Result<Vec<String>> {
        refs.iter().map(|r| self.resolve(r)).collect()
    }

    /// §5.3's conflicts: local writes that were abandoned, whose effect the
    /// next pull has undone.
    fn conflicts(&self) -> Result<Vec<mailbox_api::Conflict>> {
        Ok(self
            .store
            .failed_ops()?
            .into_iter()
            .map(|op| mailbox_api::Conflict {
                message: op.message_id,
                operation: op.kind.as_str().to_string(),
                folder: op.folder_path,
                reason: op
                    .last_error
                    .unwrap_or_else(|| "the server would not accept the change".into()),
                queued_at: format_rfc3339(op.created),
            })
            .collect())
    }

    /// One sync pass to quiescence (§6.7's `sync --once`).
    pub fn sync(&mut self, opts: &SyncOptions) -> Result<SyncResult> {
        let mut client = self.connect()?;
        let clock = self.clock.clone();
        let report = sync_once(&mut client, &mut self.store, opts, &clock);
        // Log out even when the pass failed: leaving a connection open counts
        // against the server's per-account cap (§4.1) long after we are gone.
        let _ = client.logout();
        Ok(SyncResult {
            synced: vec![report?],
        })
    }

    /// `debug store-dump` (§6.7).
    pub fn store_dump(&self, query_str: Option<&str>) -> Result<serde_json::Value> {
        let node = match query_str {
            Some(q) if !q.trim().is_empty() => self.parse_query(q)?,
            _ => Node::All,
        };
        self.store.store_dump(&node)
    }

    fn identities(&self) -> Vec<String> {
        self.account_config()
            .map(|c| c.all_identities())
            .unwrap_or_default()
    }

    /// Turns an [`IdRef`] into a full id.
    pub fn resolve(&self, id_ref: &IdRef) -> Result<String> {
        match id_ref {
            IdRef::Id(s) => self.store.resolve_id(s),
            IdRef::Previous => crate::history::History::load(&self.account_id)
                .previous()
                .ok_or_else(|| Error::not_found("previous message", "^"))
                .and_then(|id| self.store.resolve_id(&id)),
            IdRef::Index(n) => crate::history::History::load(&self.account_id)
                .index(*n)
                .ok_or_else(|| {
                    Error::usage(format!(
                        "`{n}` does not index the last listing; run `ls` first, or use an id"
                    ))
                })
                .and_then(|id| self.store.resolve_id(&id)),
            // §5.2: `@` is whatever the TUI/GUI has selected. There is no
            // running one to ask until M3, and inventing an answer would be
            // worse than saying so.
            IdRef::Selected => Err(Error::usage(
                "`@` names the message selected in the TUI or GUI, and neither is running",
            )),
        }
    }
}

/// §9: plaintext IMAP is refused unless the host is loopback and the user opted
/// in explicitly. This is the loopback half.
fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("127.")
}

/// A parsed `flag` argument list: `+seen -flagged +tag:invoices` (§6.1).
///
/// Flags and tags are separated here rather than downstream because they have
/// different fates: a flag is queued for the server, a tag is local until M8
/// gives it the state folder (§5.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlagSpec {
    pub add_flags: Vec<String>,
    pub remove_flags: Vec<String>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
}

impl FlagSpec {
    /// Parses `+seen`, `-flagged`, `+tag:invoices`.
    ///
    /// The sign is required. `mailbox flag 1 seen` is rejected rather than
    /// guessed at: it reads equally well as "set seen" and as "these are the
    /// flags now", and the two differ.
    pub fn parse(args: &[String]) -> Result<FlagSpec> {
        let mut spec = FlagSpec::default();
        for arg in args {
            let (adding, rest) = match arg.chars().next() {
                Some('+') => (true, &arg[1..]),
                Some('-') => (false, &arg[1..]),
                _ => {
                    return Err(Error::usage(format!(
                        "`{arg}` needs a sign: `+{arg}` to add it, `-{arg}` to remove it"
                    )))
                }
            };
            if rest.is_empty() {
                return Err(Error::usage("a bare `+` or `-` names no flag"));
            }

            if let Some(tag) = rest.strip_prefix("tag:") {
                if tag.is_empty() {
                    return Err(Error::usage("`tag:` needs a name"));
                }
                let tag = tag.to_ascii_lowercase();
                if adding {
                    spec.add_tags.push(tag);
                } else {
                    spec.remove_tags.push(tag);
                }
                continue;
            }

            let flag = normalize_flag_name(rest)?;
            if adding {
                spec.add_flags.push(flag);
            } else {
                spec.remove_flags.push(flag);
            }
        }
        if spec == FlagSpec::default() {
            return Err(Error::usage(
                "nothing to change; try `+seen`, `-flagged`, or `+tag:invoices`",
            ));
        }
        Ok(spec)
    }
}

/// Accepts the system flags by their short names and any server keyword.
///
/// Keywords are legitimate IMAP — servers and other clients invent them — so
/// this rejects only what cannot be sent: whitespace, and the characters that
/// would break an atom out of its parentheses.
fn normalize_flag_name(name: &str) -> Result<String> {
    let n = name.trim_start_matches('\\').to_ascii_lowercase();
    let n = match n.as_str() {
        "unread" | "unseen" => {
            return Err(Error::usage(
                "`unread` is the absence of a flag; use `-seen` to mark unread",
            ))
        }
        "read" => crate::flags::SEEN.to_string(),
        "junk" => crate::flags::SPAM.to_string(),
        other => other.to_string(),
    };
    if n.is_empty()
        || n.chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '{' | '%' | '*' | '"' | '\\'))
    {
        return Err(Error::usage(format!("`{name}` is not a usable flag name")));
    }
    Ok(n)
}

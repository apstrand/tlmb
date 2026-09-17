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
    /// Relocates the per-account store. The mobile shells pass the app's
    /// Application Support directory here rather than relying on XDG, which
    /// does not exist on iOS.
    pub state_dir: Option<PathBuf>,
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
    /// The injected RNG of §6.7. Everything that needs unpredictability —
    /// `Message-ID`s, MIME boundaries — draws from here, so a seeded run is
    /// reproducible.
    pub rng: crate::Rng,
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

/// The width of the id `ls` prints. A decimal string this long is one of those
/// — about one id in twenty-seven is all digits — not an index into a listing
/// nobody has ever made that long.
const PRINTED_ID_WIDTH: usize = 7;

impl IdRef {
    pub fn parse(s: &str) -> IdRef {
        match s {
            "@" => IdRef::Selected,
            "^" => IdRef::Previous,
            _ if s.len() >= PRINTED_ID_WIDTH => IdRef::Id(s.to_string()),
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
                .map_err(|_| Error::no_accounts())?
                .to_string(),
        };

        let dir = match &opts.state_dir {
            Some(root) => {
                // Recorded so that everything else derived from the state
                // directory — the owner socket, the OAuth2 token cache — lands
                // beside the store rather than at an XDG path that does not
                // exist on this platform.
                crate::paths::set_state_root(root);
                root.join(&account_id)
            }
            None => crate::paths::account_dir(&account_id),
        };
        let store = if opts.read_only {
            Store::open_read_only(&dir, &account_id, tracer.clone(), clock.clone())?
        } else {
            Store::open(&dir, &account_id, tracer.clone(), clock.clone())?
        };

        Ok(Session {
            config,
            account_id,
            clock,
            rng: crate::Rng::from_env(),
            tracer,
            agent_mode: opts.agent_mode,
            timeout_ms: opts.timeout_ms.unwrap_or(30_000),
            store,
        })
    }

    /// Builds the ordinary session facade around a caller-provided store.
    /// Hosted wasm restores that store from OPFS; every operation above this
    /// constructor is the same one native and mobile frontends use.
    pub fn from_store(
        config: Config,
        account_id: impl Into<String>,
        store: Store,
        tracer: Tracer,
        clock: Clock,
    ) -> Result<Session> {
        let account_id = account_id.into();
        config.account(&account_id)?;
        if store.account() != account_id {
            return Err(Error::store(format!(
                "store for `{}` cannot be opened as `{account_id}`",
                store.account()
            )));
        }
        Ok(Session {
            config,
            account_id,
            clock,
            rng: crate::Rng::from_env(),
            tracer,
            agent_mode: false,
            timeout_ms: 30_000,
            store,
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
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
            // An explicit query is answered as asked, snoozed mail included.
            // Only the *default* listing hides sleeping messages: that is what
            // makes `ls is:snoozed` mean something, and it keeps snooze from
            // becoming a filter that quietly removes mail from every search.
            Some(q) if !q.trim().is_empty() => self.parse_query(q)?,
            _ => self.parse_query("in:inbox AND NOT is:snoozed")?,
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
            // Cheap: it is read from `state.toml`, which is already loaded.
            partial: !self.store.sync_state().backfilling().is_empty(),
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
        let mut folders = self.store.folders()?;
        self.apply_folder_preferences(&mut folders);
        Ok(FolderList { folders })
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

        let mut folders = listed
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
            .collect();
        self.apply_folder_preferences(&mut folders);
        Ok(FolderList { folders })
    }

    /// Changes a folder's role on the server and immediately mirrors all
    /// server-reported roles locally. A server may move a unique role, such as
    /// `sent`, off another folder as a side effect, so refreshing only the
    /// named folder would leave the local view inconsistent.
    pub fn set_folder_use(&mut self, path: &str, use_name: Option<&str>) -> Result<Folder> {
        let mut client = self.connect()?;
        client.set_special_use(path, use_name)?;
        let listed = client.list_folders();
        let _ = client.logout();
        let listed = listed?;

        if !listed.iter().any(|folder| folder.path == path) {
            return Err(Error::not_found("folder", path));
        }
        for folder in listed {
            self.store.upsert_folder(&folder)?;
        }
        self.store
            .folders()?
            .into_iter()
            .find(|folder| folder.path == path)
            .ok_or_else(|| Error::not_found("folder", path))
    }

    /// Raises desktop notifications for mail that has just arrived (§11).
    ///
    /// On the very first sync of an account everything is marked announced and
    /// nothing is raised: adding a mailbox should not produce a notification
    /// for every message it has ever received.
    fn announce_new_mail(&mut self, first_ever_sync: bool) -> Result<()> {
        if first_ever_sync {
            self.store.mark_all_notified()?;
            return Ok(());
        }
        let config = self.config.notify.clone();
        if !config.enabled || self.agent_mode {
            // An agent driving the binary is not sitting at the desktop, and
            // should not be able to make something pop up on one (§6.4).
            self.store.mark_all_notified()?;
            return Ok(());
        }

        let node = self.parse_query(&config.query)?;
        let total = self.store.unnotified_count(&node)?;
        if total == 0 {
            return Ok(());
        }
        let pending = self.store.unnotified(&node, config.max_per_sync)?;

        let notifier = crate::notify::Notifier::discover(config.command.as_deref());
        if notifier.is_available() {
            if total as usize > config.max_per_sync {
                notifier.send(&crate::notify::Notification::for_many(total as usize))?;
            } else {
                for message in &pending {
                    notifier.send(&crate::notify::Notification::for_message(message))?;
                }
            }
        }

        // Marked regardless of whether a notifier existed. Leaving them unmarked
        // on a headless machine would make every later sync reconsider the same
        // mail forever.
        self.store.mark_all_notified()?;
        self.tracer
            .emit1(crate::trace::event::NOTIFY_SENT, "count", total);
        Ok(())
    }

    /// Writes out an L1 transcript when `MAILBOX_RECORD_PATH` asked for one.
    ///
    /// §14.2 records L1 transcripts from L2 and L4 runs, so this lives on the
    /// ordinary sync path rather than in a test harness: what gets recorded is
    /// the real client's real conversation.
    fn save_transcript(&self, client: &Client) -> Result<()> {
        let Some(path) = std::env::var_os("MAILBOX_RECORD_PATH").filter(|v| !v.is_empty()) else {
            return Ok(());
        };
        let Some(transcript) = client.transcript() else {
            return Ok(());
        };
        transcript.save(std::path::Path::new(&path))
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
                imap_security: format!("{:?}", cfg.security()).to_lowercase(),
                smtp_host: cfg.smtp_host.clone().unwrap_or_default(),
                smtp_port: cfg.smtp_port(),
                smtp_security: format!("{:?}", cfg.smtp_security()).to_lowercase(),
                smtp_user: cfg.smtp_login_user().to_string(),
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
                outbox_pending: self.store.outbox_pending_count()?,
                conflicts: self.conflicts()?,
                last_sync: (state.last_full_sync > 0).then(|| format_rfc3339(state.last_full_sync)),
                last_error: None,
                coverage: self.coverage()?,
            }],
            store_bytes: self.store.disk_usage(),
        })
    }

    /// How much of this account's mail is here yet (§5.1).
    ///
    /// A store that is still filling in has to say so everywhere it is asked,
    /// because every listing and every search it answers is a partial answer.
    pub fn coverage(&self) -> Result<mailbox_api::Coverage> {
        let state = self.store.sync_state();
        let mut backfilling = Vec::new();
        let mut remaining = 0u64;
        for (path, folder) in state.backfilling() {
            let have = self.store.folder_message_count(path)?;
            let total = folder.server_total as u64;
            remaining += total.saturating_sub(have);
            backfilling.push(mailbox_api::FolderCoverage {
                folder: path.to_string(),
                have,
                total,
            });
        }
        Ok(mailbox_api::Coverage {
            complete: backfilling.is_empty(),
            backfilling,
            remaining,
            oldest: self.store.oldest_message_date()?.map(format_rfc3339),
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

    /// Sets messages aside until `until` (§7).
    ///
    /// Purely local: nothing is queued for the server, because a snooze is not
    /// an IMAP concept and inventing one would put a private decision into a
    /// mailbox shared with every other client the user runs.
    pub fn snooze(&mut self, refs: &[IdRef], until: i64) -> Result<Vec<String>> {
        let ids = self.resolve_all(refs)?;
        let device = crate::paths::device_name();
        let now = self.clock.now();
        for id in &ids {
            self.store.snooze(id, until, &device, now)?;
        }
        Ok(ids)
    }

    /// Wakes messages now. Returns the ids that were actually asleep.
    pub fn unsnooze(&mut self, refs: &[IdRef]) -> Result<Vec<String>> {
        let ids = self.resolve_all(refs)?;
        let mut woken = Vec::new();
        for id in ids {
            if self.store.unsnooze(&id)? {
                woken.push(id);
            }
        }
        Ok(woken)
    }

    /// Everything still asleep in this account, soonest first.
    pub fn snoozed(&self) -> Result<Vec<crate::store::snooze::Snooze>> {
        self.store.snoozed(self.clock.now())
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
        self.folders()?
            .folders
            .into_iter()
            .find(|f| f.special_use.as_deref() == Some(use_name))
            .map(|f| f.path)
            .ok_or_else(|| {
                Error::not_found("folder", format!("{use_name} (this account has none)"))
            })
    }

    fn apply_folder_preferences(&self, folders: &mut Vec<Folder>) {
        let Some(account) = self.config.accounts.get(&self.account_id) else {
            return;
        };
        for (use_name, preferred_path) in &account.folder_use {
            let Some(preferred) = folders.iter().position(|folder| {
                folder.path == *preferred_path
                    && folder.special_use.as_deref() == Some(use_name.as_str())
            }) else {
                continue;
            };
            let Some(first) = folders
                .iter()
                .position(|folder| folder.special_use.as_deref() == Some(use_name.as_str()))
            else {
                continue;
            };
            if preferred != first {
                let folder = folders.remove(preferred);
                folders.insert(first, folder);
            }
        }
    }

    fn resolve_all(&self, refs: &[IdRef]) -> Result<Vec<String>> {
        refs.iter().map(|r| self.resolve(r)).collect()
    }

    /// Makes sure a message's body is in the store, fetching it if not.
    ///
    /// §5.1 fetches bodies lazily — "bodies for what you read plus a
    /// configurable prefetch window" — so opening older mail is the moment the
    /// rest of it arrives. Offline, this fails and the caller shows what it
    /// has; a message with headers and no body is a normal state, not an error.
    pub fn ensure_body(&mut self, message_id: &str) -> Result<bool> {
        if self.store.has_body(message_id)? {
            return Ok(true);
        }
        let Some((folder, uid)) = self.store.server_location(message_id)? else {
            // A message with no server placement — one moved locally and not
            // yet pushed — has nowhere to be fetched from yet.
            return Ok(false);
        };

        let mut client = self.connect()?;
        client.examine(&folder, None)?;
        let fetched = client.fetch_bodies(&[uid])?;
        let _ = client.logout();

        let Some(raw) = fetched
            .into_iter()
            .find(|f| f.uid == uid)
            .and_then(|f| f.body)
        else {
            return Ok(false);
        };
        self.store.set_body(message_id, &raw)?;
        Ok(true)
    }

    /// The decoded bytes of one MIME part, fetching the message if needed.
    pub fn attachment(
        &mut self,
        message_id: &str,
        part: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        self.ensure_body(message_id)?;
        let raw = self.store.raw_message(message_id)?;
        crate::mime::extract_part(&raw, part)
            .ok_or_else(|| Error::not_found("part", format!("{part} of {message_id}")))
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
    ///
    /// Sending comes first: a message queued a moment ago should go out on this
    /// pass, and doing it before the IMAP session means the `Sent` copy can be
    /// filed by the same connection rather than waiting for the next pass.
    ///
    /// A mail server that is down must not stop the mailbox from syncing, so a
    /// submission failure is reported and swallowed rather than propagated.
    pub fn sync(&mut self, requested: &SyncOptions) -> Result<SyncResult> {
        // §5.1's `offline` is per account, and a workspace syncs several at
        // once — so the policy is applied here, against this account's config,
        // rather than baked into the options the caller passed.
        let account = self.account_config()?.clone();
        let opts = &SyncOptions {
            full: requested.full || account.wants_everything_offline(),
            prefetch_days: if requested.full || account.wants_everything_offline() {
                None
            } else {
                requested.prefetch_days.or_else(|| account.prefetch_days())
            },
            ..requested.clone()
        };

        // §11: whether this account has ever synced decides whether new mail is
        // *news*. Read before the pass, because the pass sets it.
        let first_ever_sync = self.store.sync_state().last_full_sync == 0;

        let mut submitted = None;
        if self.store().outbox_pending_count()? > 0 {
            match self.flush_outbox() {
                Ok(report) => submitted = Some(report),
                Err(e) if crate::smtp::submit::is_soft_failure(&e) => {
                    self.tracer.emit1(
                        crate::trace::event::OUTBOX_RESULT,
                        "submission_unavailable",
                        e.to_string(),
                    );
                }
                Err(e) => return Err(e),
            }
        }

        let mut client = self.connect()?;
        let clock = self.clock.clone();
        let report = sync_once(&mut client, &mut self.store, opts, &clock);

        // File any Sent copies while the connection is already open (§5.5).
        let preferred_sent = self.special_folder("sent").ok();
        let uploaded =
            crate::smtp::upload_sent(&mut client, &mut self.store, preferred_sent.as_deref())
                .unwrap_or(0);
        if uploaded > 0 {
            submitted.get_or_insert_with(Default::default).uploaded += uploaded;
        }

        // Log out even when the pass failed: leaving a connection open counts
        // against the server's per-account cap (§4.1) long after we are gone.
        let _ = client.logout();
        self.save_transcript(&client)?;
        let report = report?;

        // A notification that fails must not fail the sync: the mail arrived,
        // the popup did not.
        if let Err(e) = self.announce_new_mail(first_ever_sync) {
            self.tracer
                .emit1(crate::trace::event::NOTIFY_FAILED, "error", e.to_string());
        }

        Ok(SyncResult {
            synced: vec![report],
            submitted,
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
pub(crate) fn is_loopback(host: &str) -> bool {
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

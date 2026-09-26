//! Several accounts as one mailbox (§1.5, §7).
//!
//! > Multi-account is invisible until you want it: one list, per-message
//! > account color, `account:` to split.
//!
//! A [`Workspace`] holds one [`Session`] per configured account and merges
//! their answers. Two properties make that cheap:
//!
//! - **Message ids are already account-scoped.** §5.2 hashes the account id
//!   into every message id, so an id names an account as well as a message and
//!   routing a write is a lookup rather than a guess.
//! - **The sort key is already total.** `(unread, date, id)` orders rows from
//!   different accounts against each other without ambiguity, so one cursor
//!   paginates the union — no per-account bookmarks, no offsets.
//!
//! One store per account is not an implementation detail to be tidied away
//! later: §4.1 makes exactly one process the sync engine *per account*, and
//! per-account stores are what let a second account be owned by a different
//! process without any shared locking.

use mailbox_api::{
    AccountList, FolderList, MessageList, MessageSummary, ShowResult, Status, SyncResult,
};

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::imap::sync::SyncOptions;
use crate::render::RenderOptions;
use crate::session::{IdRef, Session, SessionOptions};
use crate::store::read::{ListOptions, Sort};
use crate::store::Written;
use crate::trace::Tracer;

pub struct Workspace {
    accounts: Vec<Session>,
    /// True when `--account` narrowed this to one, so listings and history stay
    /// scoped to it.
    restricted: bool,
    /// The config the accounts came from, kept because a workspace with no
    /// accounts open still has one — and the interfaces that come up before
    /// any account exists still need its `[keys]` and `[general]`.
    config: crate::config::Config,
}

impl Workspace {
    /// Builds a workspace from sessions restored by a platform storage layer.
    pub fn from_sessions(config: crate::config::Config, accounts: Vec<Session>) -> Workspace {
        Workspace {
            restricted: accounts.len() == 1,
            accounts,
            config,
        }
    }

    /// Opens every configured account, or just the one `--account` named.
    ///
    /// Fails when nothing is configured, because the caller is a command that
    /// acts on mail and has nowhere to put a setup guide.
    pub fn open(opts: &SessionOptions, tracer: Tracer, clock: Clock) -> Result<Workspace> {
        Workspace::open_inner(opts, tracer, clock, false)
    }

    /// The same, but "no accounts" is a state rather than an error.
    ///
    /// The interfaces that are a *place* rather than a command — `serve`,
    /// `gui`, the TUI — have to come up on a machine where no account has been
    /// added yet: refusing to start is the one response that leaves the user
    /// with nowhere to read the instructions. They open an empty workspace,
    /// which answers every listing with nothing, and draw §9's one-line setup
    /// over it.
    pub fn open_or_empty(opts: &SessionOptions, tracer: Tracer, clock: Clock) -> Result<Workspace> {
        Workspace::open_inner(opts, tracer, clock, true)
    }

    fn open_inner(
        opts: &SessionOptions,
        tracer: Tracer,
        clock: Clock,
        allow_empty: bool,
    ) -> Result<Workspace> {
        let config = match &opts.config_path {
            Some(p) => crate::config::Config::load(p)?,
            None => crate::config::Config::load_default()?,
        };

        let ids: Vec<String> = match &opts.account {
            Some(a) => {
                // Fail here rather than at the first store access, so
                // `--account typo` says so instead of reporting an empty
                // mailbox.
                config.account(a)?;
                vec![a.clone()]
            }
            // `--account` was not given and there is nothing to default to.
            // Whether that is fatal is the caller's to say.
            None if config.accounts.is_empty() => {
                if !allow_empty {
                    return Err(Error::no_accounts());
                }
                Vec::new()
            }
            // §12's `unified_inbox`, which is what "several accounts feel like
            // one mailbox" means in practice. Turning it off makes every
            // command act on one account unless told otherwise.
            None if config.general.unified_inbox => config.accounts.keys().cloned().collect(),
            None => vec![config.default_account()?.to_string()],
        };

        let restricted = ids.len() == 1;
        let mut accounts = Vec::with_capacity(ids.len());
        for id in ids {
            let mut per_account = opts.clone();
            per_account.account = Some(id);
            accounts.push(Session::open(&per_account, tracer.clone(), clock.clone())?);
        }
        Ok(Workspace {
            accounts,
            restricted,
            config,
        })
    }

    /// The config these accounts came from, open or not.
    pub fn config(&self) -> &crate::config::Config {
        &self.config
    }

    /// True when no account is configured — the first-launch state.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn sessions(&self) -> &[Session] {
        &self.accounts
    }

    pub fn sessions_mut(&mut self) -> &mut [Session] {
        &mut self.accounts
    }

    /// The account a single-account operation acts on.
    ///
    /// Sending needs one: a message goes out through one server, from one
    /// identity.
    pub fn primary(&self) -> Result<&Session> {
        self.accounts.first().ok_or_else(Error::no_accounts)
    }

    pub fn primary_mut(&mut self) -> Result<&mut Session> {
        self.accounts.first_mut().ok_or_else(Error::no_accounts)
    }

    /// The account that owns an identity, for `send --from`.
    ///
    /// Choosing by identity rather than by position is what makes
    /// `--from work@example.com` do the obvious thing in a unified mailbox.
    pub fn session_for_identity(&self, address: &str) -> Option<&Session> {
        let wanted = address.to_ascii_lowercase();
        self.accounts.iter().find(|s| {
            s.account_config()
                .map(|c| c.all_identities().contains(&wanted))
                .unwrap_or(false)
        })
    }

    /// The scope key for the listing history behind `^` and `1..20` (§5.2).
    ///
    /// A unified listing and a per-account one are different listings, and an
    /// index into one must not resolve against the other.
    pub fn history_scope(&self) -> String {
        if self.restricted {
            self.accounts
                .first()
                .map(|s| s.account_id.clone())
                .unwrap_or_default()
        } else {
            "unified".to_string()
        }
    }

    /// `ls` (§6.1): the unified inbox by default, unread first.
    pub fn ls(&self, query: Option<&str>, opts: &ListOptions) -> Result<MessageList> {
        self.merged(opts, |session, o| session.ls(query, o.clone()))
    }

    /// `search` (§6.1), across every open account.
    pub fn search(&self, query: &str, opts: &ListOptions) -> Result<MessageList> {
        self.merged(opts, |session, o| session.search(query, o.clone()))
    }

    /// Runs a listing against every account and merges the pages.
    ///
    /// Each account is asked for a full page, then the union is sorted and
    /// truncated. Asking for `limit` from each is deliberate over-fetching: the
    /// alternative is a second round of queries whenever one account supplies
    /// most of the page, which is the common case for a mailbox with a busy
    /// account and a quiet one.
    fn merged(
        &self,
        opts: &ListOptions,
        run: impl Fn(&Session, &ListOptions) -> Result<MessageList>,
    ) -> Result<MessageList> {
        if self.accounts.len() == 1 {
            return run(&self.accounts[0], opts);
        }

        let mut rows: Vec<MessageSummary> = Vec::new();
        let mut any_cursor = false;
        let mut total = None;

        for session in &self.accounts {
            let page = run(session, opts)?;
            any_cursor |= page.cursor.is_some();
            if let Some(t) = page.total {
                *total.get_or_insert(0) += t;
            }
            rows.extend(page.messages);
        }

        sort_rows(&mut rows, opts.sort);
        let has_more = rows.len() > opts.limit || any_cursor;
        rows.truncate(opts.limit);

        // One cursor for the union: the sort key is total across accounts, so
        // "everything after this row" means the same thing in every store.
        let cursor = if has_more {
            rows.last()
                .and_then(|last| crate::store::read::encode_cursor(last, opts.sort).ok())
        } else {
            None
        };

        Ok(MessageList {
            messages: rows,
            cursor,
            total,
            // Any account still filling in makes the unified listing partial:
            // a caller acting on this page cannot tell which account a missing
            // message would have come from.
            partial: self
                .accounts
                .iter()
                .any(|s| !s.store().sync_state().backfilling().is_empty()),
        })
    }

    /// Finds the account that owns a message, and its full id.
    ///
    /// An id prefix that matches in two accounts is ambiguous, and saying so is
    /// the point: the caller most hurt by a silent pick is an agent acting on
    /// the result (§5.2).
    pub fn locate(&self, id_ref: &IdRef) -> Result<(usize, String)> {
        // `^` and `1..20` index *this* listing, which for a unified view is the
        // workspace's, not any one account's (§5.2). Resolving them here and
        // then routing the resulting id is what keeps the two scopes from
        // being read against each other.
        match id_ref {
            IdRef::Previous | IdRef::Index(_) => {
                let history = crate::history::History::load(&self.history_scope());
                let id = match id_ref {
                    IdRef::Previous => history
                        .previous()
                        .ok_or_else(|| Error::not_found("previous message", "^"))?,
                    IdRef::Index(n) => history.index(*n).ok_or_else(|| {
                        Error::usage(format!(
                            "`{n}` does not index the last listing; run `ls` first, or use an id"
                        ))
                    })?,
                    _ => unreachable!("only the positional forms reach here"),
                };
                return self.locate(&IdRef::Id(id));
            }
            _ => {}
        }

        let mut found: Option<(usize, String)> = None;
        let mut ambiguous = 0usize;
        let mut last_error: Option<Error> = None;

        for (i, session) in self.accounts.iter().enumerate() {
            match session.resolve(id_ref) {
                Ok(id) => {
                    ambiguous += 1;
                    if found.is_none() {
                        found = Some((i, id));
                    }
                }
                Err(e @ Error::Ambiguous { .. }) => return Err(e),
                Err(e) => last_error = Some(e),
            }
        }

        match (found, ambiguous) {
            (Some(hit), 1) => Ok(hit),
            (Some(_), n) => Err(Error::Ambiguous {
                prefix: match id_ref {
                    IdRef::Id(s) => s.clone(),
                    other => format!("{other:?}"),
                },
                count: n,
            }),
            // Every account said no. Report the last real reason rather than
            // inventing a new one.
            (None, _) => {
                Err(last_error.unwrap_or_else(|| Error::not_found("message", "no account has it")))
            }
        }
    }

    pub fn show(
        &self,
        id_ref: &IdRef,
        render: &RenderOptions,
        as_thread: bool,
    ) -> Result<ShowResult> {
        let (i, id) = self.locate(id_ref)?;
        self.accounts[i].show(&IdRef::Id(id), render, as_thread)
    }

    pub fn raw(&self, id_ref: &IdRef) -> Result<Vec<u8>> {
        let (i, id) = self.locate(id_ref)?;
        self.accounts[i].raw(&IdRef::Id(id))
    }

    /// Routes each message to the account that owns it, then applies the write.
    ///
    /// Grouping by account rather than looping one message at a time keeps a
    /// multi-account `flag` to one transaction per store.
    pub fn write_each(
        &mut self,
        refs: &[IdRef],
        mut apply: impl FnMut(&mut Session, &[IdRef]) -> Result<Vec<Written>>,
    ) -> Result<Vec<Written>> {
        let mut by_account: Vec<Vec<IdRef>> = vec![Vec::new(); self.accounts.len()];
        for r in refs {
            let (i, id) = self.locate(r)?;
            by_account[i].push(IdRef::Id(id));
        }

        let mut out = Vec::new();
        for (i, ids) in by_account.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            out.extend(apply(&mut self.accounts[i], ids)?);
        }
        Ok(out)
    }

    /// Sets messages aside until `until`, routing each to its account (§7).
    pub fn snooze(&mut self, refs: &[IdRef], until: i64) -> Result<Vec<String>> {
        self.route(refs, |session, ids| session.snooze(ids, until))
    }

    /// Wakes messages now. Returns only the ids that were actually asleep.
    pub fn unsnooze(&mut self, refs: &[IdRef]) -> Result<Vec<String>> {
        self.route(refs, |session, ids| session.unsnooze(ids))
    }

    /// Everything still asleep, across accounts, soonest first.
    pub fn snoozed(&self) -> Result<Vec<mailbox_api::Snoozed>> {
        let mut out = Vec::new();
        for session in &self.accounts {
            for s in session.snoozed()? {
                out.push(mailbox_api::Snoozed {
                    id: s.message_id,
                    account: session.account_id.clone(),
                    subject: s.subject,
                    until: crate::clock::format_rfc3339(s.until),
                    device: s.device,
                });
            }
        }
        out.sort_by(|a, b| a.until.cmp(&b.until).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// [`Workspace::write_each`] for writes that report ids rather than
    /// [`Written`]s.
    fn route(
        &mut self,
        refs: &[IdRef],
        mut apply: impl FnMut(&mut Session, &[IdRef]) -> Result<Vec<String>>,
    ) -> Result<Vec<String>> {
        let mut by_account: Vec<Vec<IdRef>> = vec![Vec::new(); self.accounts.len()];
        for r in refs {
            let (i, id) = self.locate(r)?;
            by_account[i].push(IdRef::Id(id));
        }
        let mut out = Vec::new();
        for (i, ids) in by_account.iter().enumerate() {
            if ids.is_empty() {
                continue;
            }
            out.extend(apply(&mut self.accounts[i], ids)?);
        }
        Ok(out)
    }

    /// Every address these accounts may send as (§7's *From* selector).
    ///
    /// The account's own address first, then its aliases, in the order the
    /// config lists them — so the default in a menu is the obvious one.
    pub fn identities(&self) -> Result<mailbox_api::IdentityList> {
        let mut identities = Vec::new();
        for session in &self.accounts {
            let config = session.account_config()?;
            for (index, address) in config.all_identities().into_iter().enumerate() {
                identities.push(mailbox_api::Identity {
                    account: session.account_id.clone(),
                    // The address itself. There is no configured display name
                    // to put in front of it yet, and inventing one from the
                    // account id would sign mail with a nickname nobody chose.
                    display: address.clone(),
                    address,
                    primary: index == 0,
                });
            }
        }
        Ok(mailbox_api::IdentityList { identities })
    }

    pub fn folders(&self) -> Result<FolderList> {
        let mut folders = Vec::new();
        for session in &self.accounts {
            folders.extend(session.folders()?.folders);
        }
        Ok(FolderList { folders })
    }

    pub fn remote_folders(&self) -> Result<FolderList> {
        let mut folders = Vec::new();
        for session in &self.accounts {
            folders.extend(session.remote_folders()?.folders);
        }
        Ok(FolderList { folders })
    }

    /// Every configured account, whether or not it is open.
    ///
    /// Reads from the first open session, because the account list comes from
    /// the config and every session has the same one.
    pub fn accounts(&self) -> Result<AccountList> {
        match self.accounts.first() {
            Some(session) => session.accounts(),
            // Nothing open means nothing configured, and that is an answer
            // rather than a failure — it is what every interface's setup guide
            // is drawn from.
            None => Ok(AccountList {
                accounts: Vec::new(),
            }),
        }
    }

    pub fn status(&self) -> Result<Status> {
        let mut accounts = Vec::new();
        let mut store_bytes = 0;
        for session in &self.accounts {
            let s = session.status()?;
            accounts.extend(s.accounts);
            store_bytes += s.store_bytes;
        }
        Ok(Status {
            accounts,
            store_bytes,
        })
    }

    /// Syncs every open account.
    ///
    /// One account failing does not stop the others: a mailbox with a broken
    /// account should still deliver the working ones' mail. The failure is
    /// reported through `status`, which is where a user looks for it anyway.
    pub fn sync(&mut self, opts: &SyncOptions) -> Result<SyncResult> {
        self.sync_watched(opts, &mut crate::imap::sync::ignore_progress())
    }

    /// [`Workspace::sync`], saying which account and folder it is on.
    ///
    /// A pass over several accounts is the one that most needs this: it is the
    /// longest, and until it finishes every account but the first looks like a
    /// mailbox that has stopped receiving mail.
    pub fn sync_watched(
        &mut self,
        opts: &SyncOptions,
        progress: &mut dyn FnMut(crate::imap::sync::Progress),
    ) -> Result<SyncResult> {
        let mut synced = Vec::new();
        let mut submitted = None;
        let mut first_error = None;

        for session in &mut self.accounts {
            match session.sync_watched(opts, progress) {
                Ok(result) => {
                    synced.extend(result.synced);
                    if let Some(s) = result.submitted {
                        let acc: &mut mailbox_api::SubmitReport =
                            submitted.get_or_insert_with(Default::default);
                        acc.attempted += s.attempted;
                        acc.sent += s.sent;
                        acc.deferred += s.deferred;
                        acc.failed += s.failed;
                        acc.uploaded += s.uploaded;
                    }
                }
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some((session.account_id.clone(), e));
                    }
                }
            }
        }

        // Every account failed, so there is nothing to report but the failure.
        if synced.is_empty() {
            if let Some((account, e)) = first_error {
                return Err(match e {
                    Error::Usage(m) => Error::Usage(format!("{account}: {m}")),
                    other => other,
                });
            }
        }
        Ok(SyncResult { synced, submitted })
    }

    pub fn store_dump(&self, query: Option<&str>) -> Result<serde_json::Value> {
        if self.accounts.len() == 1 {
            return self.accounts[0].store_dump(query);
        }
        // Keyed by account and ordered by it, so the dump stays a stable
        // golden-file oracle with several accounts open (§14.3).
        let mut out = serde_json::Map::new();
        for session in &self.accounts {
            out.insert(session.account_id.clone(), session.store_dump(query)?);
        }
        Ok(serde_json::Value::Object(out))
    }
}

/// Orders merged rows exactly as a single store would have.
///
/// This has to mirror `Sort::order_by` in the store, or a paginated unified
/// listing would disagree with itself between pages.
fn sort_rows(rows: &mut [MessageSummary], sort: Sort) {
    let seen = |m: &MessageSummary| i32::from(m.flags.iter().any(|f| f == crate::flags::SEEN));
    match sort {
        Sort::DateDesc => rows.sort_by(|a, b| b.date.cmp(&a.date).then(b.id.cmp(&a.id))),
        Sort::DateAsc => rows.sort_by(|a, b| a.date.cmp(&b.date).then(a.id.cmp(&b.id))),
        Sort::UnreadFirst => rows.sort_by(|a, b| {
            seen(a)
                .cmp(&seen(b))
                .then(b.date.cmp(&a.date))
                .then(b.id.cmp(&a.id))
        }),
    }
}

/// Every account in the workspace, for callers that only need the names.
pub fn account_ids(workspace: &Workspace) -> Vec<String> {
    workspace
        .accounts
        .iter()
        .map(|s| s.account_id.clone())
        .collect()
}

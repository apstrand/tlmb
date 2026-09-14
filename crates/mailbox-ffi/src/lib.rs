//! The mobile shell over `mailbox-core` (§3, §4, M6).
//!
//! Frontends stay thin: this crate opens a [`mailbox_core::Workspace`], runs
//! the same operations the CLI does, and serialises the answer as the
//! mailbox-api JSON contract. Swift (and later Kotlin) decode that contract
//! rather than growing a second set of types.
//!
//! Two surfaces, one implementation:
//!
//! - [`Mailbox`] is the Rust API, which the L0 tests drive.
//! - `mailbox_open` / `mailbox_exec` / `mailbox_close` is the C ABI the
//!   SwiftUI app links.

#[cfg(not(target_arch = "wasm32"))]
mod c;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mailbox_api::{
    Account, AccountList, Address, BodyFormat, ErrorResponse, FolderList, MessageList, Reply,
    SCHEMA_VERSION,
};
use mailbox_core::accounts;
use mailbox_core::clock::Clock;
use mailbox_core::compose::Draft;
use mailbox_core::config::Config;
use mailbox_core::credentials;
use mailbox_core::error::{Error, Result};
use mailbox_core::imap::sync::SyncOptions;
use mailbox_core::imap::Credential;
use mailbox_core::providers;
use mailbox_core::render::RenderOptions;
use mailbox_core::send::{ReplyMode, SendRequest};
use mailbox_core::session::{FlagSpec, IdRef, SessionOptions};
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::trace::Tracer;
use mailbox_core::Workspace;
use serde::Deserialize;
use serde_json::{json, Value};

/// One opened mailbox. Cheap to hold; the store is opened lazily per account
/// inside the workspace, and a process with no accounts configured is a valid
/// mailbox — that is the first-launch state of the iOS app.
pub struct Mailbox {
    config_path: PathBuf,
    state_dir: PathBuf,
    inner: Mutex<Option<Workspace>>,
}

impl Mailbox {
    /// Wraps stores opened by a platform layer instead of by filesystem path.
    pub fn from_workspace(workspace: Workspace) -> Mailbox {
        Mailbox {
            config_path: PathBuf::new(),
            state_dir: PathBuf::new(),
            inner: Mutex::new(Some(workspace)),
        }
    }

    pub fn open(config_path: impl AsRef<Path>, state_dir: impl AsRef<Path>) -> Result<Mailbox> {
        let config_path = config_path.as_ref().to_path_buf();
        let state_dir = state_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&state_dir)?;
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mb = Mailbox {
            config_path,
            state_dir,
            inner: Mutex::new(None),
        };
        mb.reload()?;
        Ok(mb)
    }

    fn opts(&self) -> SessionOptions {
        SessionOptions {
            config_path: Some(self.config_path.clone()),
            state_dir: Some(self.state_dir.clone()),
            ..Default::default()
        }
    }

    /// Re-reads the config and reopens every account. Adding or removing an
    /// account is not a live mutation of the workspace — the workspace is
    /// rebuilt, which is the same shape the CLI has when you edit config.toml
    /// and run the next command.
    pub fn reload(&self) -> Result<()> {
        let config = Config::load(&self.config_path)?;
        let mut guard = self.lock()?;
        if config.accounts.is_empty() {
            *guard = None;
            return Ok(());
        }
        let ws = Workspace::open(&self.opts(), Tracer::disabled(), Clock::from_env())?;
        *guard = Some(ws);
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Workspace>>> {
        self.inner
            .lock()
            .map_err(|_| Error::store("mailbox lock poisoned"))
    }

    fn with_ws<T>(&self, f: impl FnOnce(&Workspace) -> Result<T>) -> Result<T> {
        let guard = self.lock()?;
        match guard.as_ref() {
            Some(ws) => f(ws),
            None => Err(Error::no_accounts()),
        }
    }

    fn with_ws_mut<T>(&self, f: impl FnOnce(&mut Workspace) -> Result<T>) -> Result<T> {
        let mut guard = self.lock()?;
        match guard.as_mut() {
            Some(ws) => f(ws),
            None => Err(Error::no_accounts()),
        }
    }

    pub fn list(
        &self,
        query: Option<&str>,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<MessageList> {
        let searching = query.map(str::trim).filter(|q| !q.is_empty());
        let opts = ListOptions {
            limit,
            cursor,
            sort: if searching.is_some() {
                Sort::DateDesc
            } else {
                Sort::UnreadFirst
            },
            ..Default::default()
        };
        match self.with_ws(|ws| {
            if let Some(q) = searching {
                ws.search(q, &opts)
            } else {
                ws.ls(None, &opts)
            }
        }) {
            Err(Error::NotFound {
                kind: "account", ..
            }) => Ok(MessageList {
                messages: Vec::new(),
                cursor: None,
                total: Some(0),
                // No account, so nothing is missing from what was asked for.
                partial: false,
            }),
            other => other,
        }
    }

    pub fn show(&self, id: &str, as_thread: bool, format: BodyFormat) -> Result<Value> {
        let render = RenderOptions {
            format,
            ..Default::default()
        };
        let result = self.with_ws(|ws| ws.show(&IdRef::Id(id.to_string()), &render, as_thread))?;
        Ok(serde_json::to_value(result)?)
    }

    pub fn flag(&self, ids: &[String], changes: &[String]) -> Result<Value> {
        let spec = FlagSpec::parse(&changes.iter().map(|s| s.to_string()).collect::<Vec<_>>())?;
        let refs: Vec<IdRef> = ids.iter().cloned().map(IdRef::Id).collect();
        let written = self.with_ws_mut(|ws| ws.write_each(&refs, |s, ids| s.flag(ids, &spec)))?;
        Ok(json!({ "changed": written.iter().filter(|w| w.changed).count() }))
    }

    pub fn archive(&self, ids: &[String]) -> Result<Value> {
        self.move_special(ids, "archive")
    }

    pub fn trash(&self, ids: &[String]) -> Result<Value> {
        self.move_special(ids, "trash")
    }

    pub fn move_to(&self, ids: &[String], folder: &str) -> Result<Value> {
        let refs: Vec<IdRef> = ids.iter().cloned().map(IdRef::Id).collect();
        let written = self.with_ws_mut(|workspace| {
            workspace.write_each(&refs, |session, ids| session.move_to(ids, folder))
        })?;
        Ok(json!({ "changed": written.iter().filter(|item| item.changed).count() }))
    }

    fn move_special(&self, ids: &[String], special: &str) -> Result<Value> {
        let refs: Vec<IdRef> = ids.iter().cloned().map(IdRef::Id).collect();
        let written = self.with_ws_mut(|ws| {
            ws.write_each(&refs, |s, ids| {
                let target = s.special_folder(special)?;
                s.move_to(ids, &target)
            })
        })?;
        Ok(json!({ "changed": written.iter().filter(|w| w.changed).count() }))
    }

    pub fn folders(&self) -> Result<FolderList> {
        match self.with_ws(|ws| ws.folders()) {
            Err(Error::NotFound {
                kind: "account", ..
            }) => Ok(FolderList {
                folders: Vec::new(),
            }),
            other => other,
        }
    }

    pub fn accounts(&self) -> Result<AccountList> {
        if self.config_path.as_os_str().is_empty() {
            return self.with_ws(|workspace| workspace.accounts());
        }
        let config = Config::load(&self.config_path)?;
        if config.accounts.is_empty() {
            return Ok(AccountList {
                accounts: Vec::new(),
            });
        }
        match self.with_ws(|ws| ws.accounts()) {
            Ok(list) => Ok(list),
            Err(_) => {
                // The workspace could not open (a store error on one account)
                // but the config is still the source of truth for "what is
                // configured".
                let accounts = config
                    .accounts
                    .iter()
                    .map(|(id, cfg)| Account {
                        id: id.clone(),
                        address: cfg.address.clone(),
                        login_user: cfg.login_user().to_string(),
                        imap_host: cfg.imap_host.clone().unwrap_or_default(),
                        imap_port: cfg.port(),
                        has_credential: credentials::resolve(id, cfg).is_ok(),
                        last_sync: None,
                    })
                    .collect();
                Ok(AccountList { accounts })
            }
        }
    }

    pub fn status(&self) -> Result<Value> {
        match self.with_ws(|ws| ws.status()) {
            Ok(s) => Ok(serde_json::to_value(s)?),
            Err(Error::NotFound {
                kind: "account", ..
            }) => Ok(json!({
                "accounts": [],
                "store_bytes": 0,
            })),
            Err(e) => Err(e),
        }
    }

    pub fn identities(&self) -> Result<Value> {
        Ok(serde_json::to_value(
            self.with_ws(|workspace| workspace.identities())?,
        )?)
    }

    pub fn snooze(&self, ids: &[String], until: Option<&str>) -> Result<Value> {
        let refs: Vec<IdRef> = ids.iter().cloned().map(IdRef::Id).collect();
        self.with_ws_mut(|workspace| match until {
            Some(until) => {
                let when = mailbox_core::store::snooze::parse_until(until, &Clock::from_env())?;
                Ok(json!({
                    "snoozed": workspace.snooze(&refs, when)?,
                    "until": mailbox_core::clock::format_rfc3339(when),
                    "device": mailbox_core::paths::device_name(),
                }))
            }
            None => Ok(json!({ "woke": workspace.unsnooze(&refs)? })),
        })
    }

    pub fn snoozed(&self) -> Result<Value> {
        Ok(json!({ "snoozed": self.with_ws(|workspace| workspace.snoozed())? }))
    }

    pub fn outbox(&self) -> Result<Value> {
        self.with_ws(|workspace| {
            let mut messages = Vec::new();
            for session in workspace.sessions() {
                for entry in session.store().outbox_entries(None)? {
                    messages.push(mailbox_core::send::outbox_item(session.store(), &entry));
                }
            }
            Ok(json!({ "messages": messages }))
        })
    }

    /// Serializes the store in a single-account platform workspace.
    pub fn database_snapshot(&self) -> Result<Vec<u8>> {
        self.with_ws(|workspace| {
            let session = workspace
                .sessions()
                .first()
                .ok_or_else(Error::no_accounts)?;
            session.store().serialize()
        })
    }

    pub fn sync(&self, full: bool) -> Result<Value> {
        let opts = SyncOptions {
            full,
            ..Default::default()
        };
        let result = self.with_ws_mut(|ws| ws.sync(&opts))?;
        Ok(serde_json::to_value(result)?)
    }

    pub fn ensure_body(&self, id: &str) -> Result<Value> {
        let fetched = self.with_ws_mut(|ws| {
            let (i, full) = ws.locate(&IdRef::Id(id.to_string()))?;
            ws.sessions_mut()[i].ensure_body(&full)
        })?;
        Ok(json!({ "fetched": fetched }))
    }

    pub fn send(&self, args: SendArgs) -> Result<Value> {
        let from = match args
            .from
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => Some(parse_one_address(s)?),
            None => None,
        };
        let draft = Draft {
            from,
            to: parse_address_list(&args.to)?,
            cc: parse_address_list(&args.cc)?,
            bcc: parse_address_list(&args.bcc)?,
            subject: args.subject.unwrap_or_default(),
            body: args.body.unwrap_or_default(),
            in_reply_to: args.in_reply_to,
            references: args.references.unwrap_or_default(),
            ..Default::default()
        };
        let from_addr = draft.from.as_ref().map(|a| a.address.clone());
        let result = self.with_ws_mut(|ws| {
            let session = match from_addr.as_deref() {
                Some(addr) => {
                    // session_for_identity returns &Session; we need mut. Locate
                    // by identity among the open accounts.
                    let idx = ws
                        .sessions()
                        .iter()
                        .position(|s| {
                            s.account_config()
                                .map(|c| {
                                    c.all_identities()
                                        .iter()
                                        .any(|i| i.eq_ignore_ascii_case(addr))
                                })
                                .unwrap_or(false)
                        })
                        .ok_or_else(|| Error::not_found("identity", addr))?;
                    &mut ws.sessions_mut()[idx]
                }
                None => ws.primary_mut()?,
            };
            session.enqueue_send(&SendRequest {
                draft,
                delay_seconds: 0,
                confirmed: true,
                ..Default::default()
            })
        })?;
        Ok(serde_json::to_value(result)?)
    }

    pub fn reply_draft(&self, id: &str, all: bool, forward: bool) -> Result<Value> {
        let mode = if forward {
            ReplyMode::Forward
        } else if all {
            ReplyMode::All
        } else {
            ReplyMode::Sender
        };
        let draft = self.with_ws(|ws| {
            let (i, full) = ws.locate(&IdRef::Id(id.to_string()))?;
            ws.sessions()[i].reply_draft(&IdRef::Id(full), mode)
        })?;
        Ok(json!({
            "from": draft.from,
            "to": draft.to,
            "cc": draft.cc,
            "bcc": draft.bcc,
            "subject": draft.subject,
            "body": draft.body,
            "in_reply_to": draft.in_reply_to,
            "references": draft.references,
        }))
    }

    pub fn set_password(&self, account_id: &str, password: &str) -> Result<Value> {
        credentials::install(account_id, Credential::Password(password.to_string()));
        Ok(json!({ "ok": true }))
    }

    /// Adds an account. The rules live in `mailbox_core::accounts` (§9), so
    /// the iOS setup screen, the CLI, and any future window agree about what a
    /// valid account is instead of each having an opinion.
    pub fn add_account(&self, spec: AddAccount) -> Result<Value> {
        let added = accounts::add(&self.config_path, &spec.into())?;
        self.reload()?;
        Ok(serde_json::to_value(added)?)
    }

    pub fn remove_account(&self, id: &str) -> Result<Value> {
        let store = accounts::remove(&self.config_path, id)?;
        self.reload()?;
        Ok(json!({ "removed": id, "store": store.display().to_string() }))
    }

    /// The built-in table, and nothing else.
    ///
    /// Deliberately offline: a setup screen calls this as the address is
    /// typed, and a network round trip per keystroke is both slow and a way
    /// to leak half-typed domains. [`Self::discover`] is the full chain, for
    /// when the shell has a whole address and the person has asked.
    pub fn providers(&self, address: &str) -> Result<Value> {
        match providers::hints_for(address) {
            Some(h) => Ok(json!({
                "known": true,
                "imap_host": h.imap_host,
                "imap_port": h.imap_port,
                "imap_security": h.imap_security,
                "smtp_host": h.smtp_host,
                "smtp_port": h.smtp_port,
                "smtp_security": h.smtp_security,
            })),
            None => Ok(json!({ "known": false })),
        }
    }

    /// Looks up an unknown domain's servers on the network (§9). Unlike
    /// `providers` this blocks on DNS, so the shell calls it from a worker and
    /// behind an explicit action rather than on every keystroke.
    pub fn discover(&self, address: &str) -> Result<Value> {
        let discovery = mailbox_core::discover::discover(
            address,
            &mailbox_core::discover::Options::from_env(),
        )?;
        Ok(serde_json::to_value(discovery)?)
    }

    /// Dispatches a named command with a JSON argument object. This is the
    /// C ABI's one function, and it is also what a test can call without
    /// going through `extern "C"`.
    pub fn exec(
        &self,
        method: &str,
        args_json: &str,
    ) -> std::result::Result<String, ErrorResponse> {
        match self.exec_inner(method, args_json) {
            Ok((command, payload)) => {
                let reply = Reply {
                    schema: SCHEMA_VERSION.to_string(),
                    command: command.to_string(),
                    payload,
                };
                serde_json::to_string(&reply).map_err(|e| {
                    ErrorResponse::new(mailbox_api::ErrorCode::Failed, format!("json: {e}"))
                })
            }
            Err(e) => Err(e.to_response()),
        }
    }

    fn exec_inner(&self, method: &str, args_json: &str) -> Result<(&'static str, Value)> {
        let args: Value = if args_json.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(args_json)
                .map_err(|e| Error::usage(format!("arguments are not JSON: {e}")))?
        };
        match method {
            "ls" | "list" => {
                let a: ListArgs = serde_from_value(args)?;
                Ok((
                    "ls",
                    serde_json::to_value(self.list(
                        a.query.as_deref(),
                        a.limit.unwrap_or(50),
                        a.cursor,
                    )?)?,
                ))
            }
            "search" => {
                let a: ListArgs = serde_from_value(args)?;
                let q = a.query.unwrap_or_default();
                Ok((
                    "search",
                    serde_json::to_value(self.list(Some(&q), a.limit.unwrap_or(50), a.cursor)?)?,
                ))
            }
            "show" => {
                let a: ShowArgs = serde_from_value(args)?;
                let format = match a.format.as_deref() {
                    Some("html") => BodyFormat::Html,
                    Some("md") => BodyFormat::Md,
                    Some("raw") => BodyFormat::Raw,
                    _ => BodyFormat::Text,
                };
                Ok(("show", self.show(&a.id, a.thread.unwrap_or(false), format)?))
            }
            "flag" => {
                let a: FlagArgs = serde_from_value(args)?;
                Ok(("flag", self.flag(&a.ids, &a.changes)?))
            }
            "archive" => {
                let a: IdsArgs = serde_from_value(args)?;
                Ok(("archive", self.archive(&a.ids)?))
            }
            "rm" | "trash" => {
                let a: IdsArgs = serde_from_value(args)?;
                Ok(("rm", self.trash(&a.ids)?))
            }
            "move" => {
                let a: MoveArgs = serde_from_value(args)?;
                Ok(("mv", self.move_to(&a.ids, &a.folder)?))
            }
            "folders" => Ok(("folders", serde_json::to_value(self.folders()?)?)),
            "accounts" => Ok(("accounts", serde_json::to_value(self.accounts()?)?)),
            "identities" => Ok(("identities", self.identities()?)),
            "status" => Ok(("status", self.status()?)),
            "snooze" => {
                let a: SnoozeArgs = serde_from_value(args)?;
                Ok(("snooze", self.snooze(&a.ids, a.until.as_deref())?))
            }
            "snoozed" => Ok(("snooze", self.snoozed()?)),
            "outbox" => Ok(("outbox", self.outbox()?)),
            "sync" => {
                let a: SyncArgs = serde_from_value(args)?;
                Ok(("sync", self.sync(a.full.unwrap_or(false))?))
            }
            "ensure_body" => {
                let a: IdArgs = serde_from_value(args)?;
                Ok(("show", self.ensure_body(&a.id)?))
            }
            "send" => {
                let a: SendArgs = serde_from_value(args)?;
                Ok(("send", self.send(a)?))
            }
            "reply_draft" => {
                let a: ReplyArgs = serde_from_value(args)?;
                Ok((
                    "compose",
                    self.reply_draft(&a.id, a.all.unwrap_or(false), a.forward.unwrap_or(false))?,
                ))
            }
            "set_password" => {
                let a: PasswordArgs = serde_from_value(args)?;
                Ok(("accounts", self.set_password(&a.account, &a.password)?))
            }
            "accounts.add" | "add_account" => {
                let a: AddAccount = serde_from_value(args)?;
                Ok(("accounts", self.add_account(a)?))
            }
            "accounts.remove" | "remove_account" => {
                let a: IdArgs = serde_from_value(args)?;
                let id = a.id;
                Ok(("accounts", self.remove_account(&id)?))
            }
            "providers" => {
                let a: AddressArgs = serde_from_value(args)?;
                Ok(("accounts", self.providers(&a.address)?))
            }
            "discover" => {
                let a: AddressArgs = serde_from_value(args)?;
                Ok(("accounts", self.discover(&a.address)?))
            }
            other => Err(Error::usage(format!("unknown method `{other}`"))),
        }
    }
}

fn serde_from_value<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error::usage(format!("arguments: {e}")))
}

fn parse_address_list(raw: &[String]) -> Result<Vec<Address>> {
    let mut out = Vec::new();
    for item in raw {
        for part in item.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            out.push(parse_one_address(part)?);
        }
    }
    Ok(out)
}

fn parse_one_address(s: &str) -> Result<Address> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::usage("empty address"));
    }
    if let Some(start) = s.find('<') {
        if let Some(end) = s.rfind('>') {
            let addr = s[start + 1..end].trim();
            if !addr.contains('@') {
                return Err(Error::usage(format!("`{s}` is not an email address")));
            }
            let name = s[..start].trim().trim_matches('"');
            return Ok(Address::new(
                if name.is_empty() {
                    None
                } else {
                    Some(name.to_string())
                },
                addr,
            ));
        }
    }
    if !s.contains('@') {
        return Err(Error::usage(format!("`{s}` is not an email address")));
    }
    Ok(Address::new(None, s))
}

#[derive(Debug, Deserialize)]
struct ListArgs {
    query: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShowArgs {
    id: String,
    thread: Option<bool>,
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlagArgs {
    ids: Vec<String>,
    changes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IdsArgs {
    ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct IdArgs {
    id: String,
}

#[derive(Debug, Deserialize)]
struct MoveArgs {
    ids: Vec<String>,
    folder: String,
}

#[derive(Debug, Deserialize)]
struct SnoozeArgs {
    ids: Vec<String>,
    until: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SyncArgs {
    full: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ReplyArgs {
    id: String,
    all: Option<bool>,
    forward: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PasswordArgs {
    account: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct AddressArgs {
    address: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SendArgs {
    pub from: Option<String>,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AddAccount {
    #[serde(default)]
    pub id: String,
    pub address: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<String>,
    pub imap_user: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<String>,
    pub smtp_user: Option<String>,
    pub password: Option<String>,
    pub require_tls: Option<bool>,
    #[serde(default)]
    pub oauth_provider: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// Look the servers up on the network when none are given (§9). Off by
    /// default; `discover` is the method that does it visibly.
    #[serde(default)]
    pub discover: bool,
}

/// The shell's wire shape is the core's, field for field; keeping the two
/// types distinct is what lets the FFI stay a stable ABI while the core's grows
/// a field.
impl From<AddAccount> for accounts::NewAccount {
    fn from(a: AddAccount) -> accounts::NewAccount {
        accounts::NewAccount {
            // The shell decides: a setup screen calls `discover` while the
            // person is still looking at the form, and fills the fields in
            // where they can see them, rather than having `add` reach the
            // network behind a spinner.
            discover: a.discover,
            id: a.id,
            address: a.address,
            imap_host: a.imap_host,
            imap_port: a.imap_port,
            imap_security: a.imap_security,
            imap_user: a.imap_user,
            smtp_host: a.smtp_host,
            smtp_port: a.smtp_port,
            smtp_security: a.smtp_security,
            smtp_user: a.smtp_user,
            password: a.password,
            credential_command: None,
            require_tls: a.require_tls,
            oauth_provider: a.oauth_provider,
            oauth_client_id: a.oauth_client_id,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use c::{mailbox_close, mailbox_exec, mailbox_open, mailbox_string_free};

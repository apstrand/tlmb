//! The §6.4 contract, over HTTP.
//!
//! Every route here answers with the same types `--json` prints, because §4 is
//! explicit that there is one contract and the CLI's output *is* it:
//!
//! > The CLI's `--json` output *is* this contract, so anything the GUI can do,
//! > a script can do.
//!
//! What is deliberately absent is a route that runs a command. Handing a
//! browser an `argv` endpoint would have been a dozen lines and would have made
//! every future subcommand — `pair send`, `|` to a shell — reachable from a
//! page. The routes are named one at a time, and adding one is a decision.
//!
//! Two of them are about setting the mailbox up rather than reading it —
//! `providers` and `POST accounts` — because §9's "setup is one line" is a
//! line typed in a terminal, and the two interfaces served from here do not
//! have one. Without them a browser or a desktop window is a mail client that
//! can only be started by someone who did not need it. They are the only
//! routes a non-loopback server refuses: see `setup_allowed` in `server`.
//!
//! The one other route that writes the config is `settings`, which holds the
//! interface's own preferences — what a swipe does. It carries nothing a
//! passer-by could learn from, so it is served wherever the page is.

use std::sync::Mutex;

use mailbox_api::BodyFormat;
use mailbox_core::error::{Error, Result};
use mailbox_core::render::RenderOptions;
use mailbox_core::session::{FlagSpec, IdRef, Session, SessionOptions};
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::{Clock, Tracer, Workspace};

use crate::http::{Request, Response};

/// What the sync pass running right now is doing (§7).
///
/// A sync over several accounts and their folders takes long enough that a
/// spinner alone is indistinguishable from a hang. The pass reports the
/// boundaries it already has, this holds the latest one, and the page reads it
/// while its own request is still in flight — which works because the server
/// serves each connection on its own thread and this lock is never held across
/// any I/O.
///
/// No history and no queue: the only question being answered is "what now?",
/// and an interface that fell behind would rather skip to the present than
/// replay where the sync has been.
#[derive(Default)]
pub struct SyncProgress {
    stage: Mutex<Option<mailbox_core::imap::sync::Progress>>,
}

impl SyncProgress {
    pub fn new() -> SyncProgress {
        SyncProgress::default()
    }

    fn set(&self, stage: mailbox_core::imap::sync::Progress) {
        if let Ok(mut held) = self.stage.lock() {
            *held = Some(stage);
        }
    }

    fn clear(&self) {
        if let Ok(mut held) = self.stage.lock() {
            *held = None;
        }
    }

    /// `{"running": false}` when nothing is syncing, and the account and
    /// folder when something is.
    fn snapshot(&self) -> serde_json::Value {
        let held = self.stage.lock().ok().and_then(|s| s.clone());
        match held {
            Some(stage) => serde_json::json!({
                "running": true,
                "account": stage.account,
                "folder": stage.folder,
            }),
            None => serde_json::json!({ "running": false }),
        }
    }
}

/// Clears the stage however the pass ends, including by `?` and by panic.
struct Running<'a>(&'a SyncProgress);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.clear();
    }
}

#[allow(clippy::too_many_arguments)]
pub fn handle(
    path: &str,
    request: &Request,
    workspace: &Mutex<Workspace>,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    setup_allowed: bool,
    progress: &SyncProgress,
) -> Response {
    let result = match (request.method.as_str(), path) {
        ("GET", "ls") => list(request, workspace),
        ("GET", "search") => list(request, workspace),
        ("GET", "show") => show(request, workspace),
        ("GET", "folders") => reply(workspace.lock().unwrap().folders()),
        ("GET", "accounts") => reply(workspace.lock().unwrap().accounts()),
        ("GET", "status") => reply(workspace.lock().unwrap().status()),
        ("GET", "identities") => reply(workspace.lock().unwrap().identities()),
        ("GET", "reply") => reply_draft(request, workspace),
        ("GET", "outbox") => outbox(workspace),
        ("POST", "send") => send(request, session, tracer, clock),
        ("GET", "snoozed") => reply(
            workspace
                .lock()
                .unwrap()
                .snoozed()
                .map(|snoozed| mailbox_api::SnoozeList { snoozed }),
        ),
        ("POST", "flag") => flag(request, session, tracer, clock),
        ("POST", "move") => move_to(request, session, tracer, clock),
        ("POST", "snooze") => snooze(request, session, tracer, clock),
        ("POST", "body") => fetch_body(request, session, tracer, clock),
        ("POST", "sync") => sync(request, workspace, session, tracer, clock, progress),
        // Read while a sync is still running, from another connection.
        ("GET", "sync/progress") => Ok(Response::json(200, &progress.snapshot())),
        ("GET", "providers") => providers(request),
        ("GET", "discover") => discover(request),
        ("POST", "accounts") => {
            add_account(request, workspace, session, tracer, clock, setup_allowed)
        }
        ("POST", "accounts/edit") => {
            edit_account(request, workspace, session, tracer, clock, setup_allowed)
        }
        ("POST", "accounts/test") => test_account(request, tracer, setup_allowed),
        ("GET", "settings") => settings(session),
        ("POST", "settings") => save_settings(request, session),
        ("GET", _) | ("POST", _) => Err(Error::not_found("route", path)),
        _ => Err(Error::usage(format!(
            "{} is not a method this serves",
            request.method
        ))),
    };

    match result {
        Ok(response) => response,
        // The same error shape the CLI writes to stderr (§6.5), so a caller
        // that knows one knows the other.
        Err(e) => Response::json(
            status_for(&e),
            &serde_json::to_value(e.to_response()).unwrap_or_default(),
        ),
    }
}

fn status_for(e: &Error) -> u16 {
    match e {
        Error::NotFound { .. } => 404,
        Error::Usage(_) | Error::Ambiguous { .. } => 400,
        Error::PolicyDenied { .. } | Error::NeedsConfirmation { .. } => 403,
        // A mail server refusing a password is not this server failing, and
        // the setup form needs the difference: "your password is wrong" and
        // "the machine running this is broken" are different things to show
        // someone. The 401 here is about the *mail* account, and carries
        // `auth_failed` in the body to say so — the missing-token 401 is
        // written in `server` and never reaches this function.
        Error::Auth(_) => 401,
        Error::Network(_) | Error::Timeout(_) => 504,
        _ => 500,
    }
}

/// Serialises any contract type into the reply envelope.
fn reply<T: serde::Serialize>(value: Result<T>) -> Result<Response> {
    Ok(Response::json(200, &serde_json::to_value(value?)?))
}

fn list(request: &Request, workspace: &Mutex<Workspace>) -> Result<Response> {
    let query = request.query.get("q").cloned().unwrap_or_default();
    let opts = ListOptions {
        limit: request
            .query
            .get("limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(50)
            // A page is a page. Without a cap, one request can ask for the
            // whole mailbox and hold the store's lock while it builds it.
            .min(500),
        cursor: request.query.get("cursor").cloned(),
        sort: if query.trim().is_empty() {
            Sort::UnreadFirst
        } else {
            Sort::DateDesc
        },
        ..Default::default()
    };

    let workspace = workspace.lock().unwrap();
    let list = if query.trim().is_empty() {
        workspace.ls(None, &opts)?
    } else {
        workspace.search(&query, &opts)?
    };
    Ok(Response::json(200, &serde_json::to_value(list)?))
}

fn show(request: &Request, workspace: &Mutex<Workspace>) -> Result<Response> {
    let id = request
        .query
        .get("id")
        .ok_or_else(|| Error::usage("show needs an `id`"))?;
    let format = match request.query.get("format").map(String::as_str) {
        Some("text") => BodyFormat::Text,
        Some("html") => BodyFormat::Html,
        // Markdown by default, as the TUI takes it: the structure of HTML mail
        // survives, and the shared UI styles it.
        None | Some("md") => BodyFormat::Md,
        Some(other) => {
            return Err(Error::usage(format!(
                "`{other}` is not a body format; try text, md, or html"
            )))
        }
    };
    let as_thread = request.query.get("thread").is_some_and(|v| v == "1");

    let render = RenderOptions {
        format,
        ..Default::default()
    };
    let workspace = workspace.lock().unwrap();
    let result = workspace.show(&IdRef::parse(id), &render, as_thread)?;
    Ok(Response::json(200, &serde_json::to_value(result)?))
}

/// What is queued to go out (§5.5).
///
/// A compose window that says "sent" and then loses the message is worse than
/// one that says "queued", so the interface has somewhere to show the queue.
fn outbox(workspace: &Mutex<Workspace>) -> Result<Response> {
    let workspace = workspace.lock().unwrap();
    let mut messages = Vec::new();
    for session in workspace.sessions() {
        for entry in session.store().outbox_entries(None)? {
            messages.push(mailbox_core::send::outbox_item(session.store(), &entry));
        }
    }
    Ok(Response::json(
        200,
        &serde_json::to_value(mailbox_api::OutboxList { messages })?,
    ))
}

/// The draft a reply, reply-all, or forward starts from (§7).
///
/// Built by the core, which owns what quoting looks like and which addresses a
/// reply-all keeps — the interface should not be re-deriving either, and two
/// interfaces deriving them differently is how "reply" starts meaning
/// different things in different windows.
fn reply_draft(request: &Request, workspace: &Mutex<Workspace>) -> Result<Response> {
    use mailbox_core::send::ReplyMode;

    let id = request
        .query
        .get("id")
        .ok_or_else(|| Error::usage("reply needs an `id`"))?;
    let mode = match request.query.get("mode").map(String::as_str) {
        None | Some("sender") => ReplyMode::Sender,
        Some("all") => ReplyMode::All,
        Some("forward") => ReplyMode::Forward,
        Some(other) => {
            return Err(Error::usage(format!(
                "`{other}` is not a reply mode; try sender, all, or forward"
            )))
        }
    };

    let workspace = workspace.lock().unwrap();
    let (index, id) = workspace.locate(&IdRef::parse(id))?;
    let draft = workspace.sessions()[index].reply_draft(&IdRef::Id(id), mode)?;
    Ok(Response::json(
        200,
        &serde_json::to_value(mailbox_core::compose::spec_from_draft(&draft))?,
    ))
}

/// Queues a message (§5.5).
///
/// Queues, never sends: the outbox is flushed by a sync pass or by
/// `outbox send`. That separation is what makes the undo-send window and the
/// approval queue possible, and what makes a crash between "queued" and "sent"
/// recoverable rather than ambiguous — none of which the interface should have
/// to know about.
fn send(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let spec: mailbox_api::DraftSpec = serde_json::from_slice(&request.body)
        .map_err(|e| Error::usage(format!("the request body is not a draft: {e}")))?;
    let draft = mailbox_core::compose::draft_from_spec(&spec)?;

    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        return Err(Error::usage("a message needs at least one recipient"));
    }

    let mut session = writable_session(session, tracer, clock)?;
    let result = session.enqueue_send(&mailbox_core::send::SendRequest {
        draft,
        idempotency_key: None,
        delay_seconds: 0,
        dry_run: false,
        // §6.5's external-recipient confirmation is answered by the person
        // clicking Send in a window that shows them the recipients.
        confirmed: true,
        draft_only: false,
    })?;
    Ok(Response::json(200, &serde_json::to_value(result)?))
}

/// Opens a writable workspace for one operation.
///
/// Per request rather than held: the server's own handle is read-only, so
/// reads never contend with a write, and a write that fails leaves nothing
/// half-open (§4.1).
///
/// Every account, not one. The page shows the unified mailbox of §1.5, so the
/// message being written to belongs to whichever account owns its id, and only
/// a workspace can route it there. Opening a single session instead resolves
/// every id against the default account's store, which makes a write to any
/// other account fail as "no such message" — naming a message the page is
/// displaying at that moment.
fn writable(session: &SessionOptions, tracer: &Tracer, clock: &Clock) -> Result<Workspace> {
    let mut opts = session.clone();
    opts.read_only = false;
    Workspace::open(&opts, tracer.clone(), clock.clone())
}

/// The same, for the writes that belong to one account rather than to the
/// message they name.
fn writable_session(session: &SessionOptions, tracer: &Tracer, clock: &Clock) -> Result<Session> {
    let mut opts = session.clone();
    opts.read_only = false;
    Session::open(&opts, tracer.clone(), clock.clone())
}

fn body(request: &Request) -> Result<serde_json::Value> {
    serde_json::from_slice(&request.body)
        .map_err(|e| Error::usage(format!("the request body is not JSON: {e}")))
}

fn ids(value: &serde_json::Value) -> Result<Vec<IdRef>> {
    let list = value
        .get("ids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::usage("give `ids` as an array of message ids"))?;
    if list.is_empty() {
        return Err(Error::usage("no message given"));
    }
    Ok(list
        .iter()
        .filter_map(|v| v.as_str())
        .map(IdRef::parse)
        .collect())
}

fn strings(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn flag(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let refs = ids(&body)?;
    let spec = FlagSpec {
        add_flags: strings(&body, "add"),
        remove_flags: strings(&body, "remove"),
        add_tags: strings(&body, "add_tags"),
        remove_tags: strings(&body, "remove_tags"),
    };

    let mut workspace = writable(session, tracer, clock)?;
    let written = workspace.write_each(&refs, |session, ids| session.flag(ids, &spec))?;
    Ok(Response::json(
        200,
        &serde_json::to_value(mailbox_core::store::write_result(&written))?,
    ))
}

fn move_to(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let refs = ids(&body)?;
    let folder = body
        .get("folder")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::usage("give a `folder` to move to"))?;

    let mut workspace = writable(session, tracer, clock)?;
    let written = workspace.write_each(&refs, |session, ids| session.move_to(ids, folder))?;
    Ok(Response::json(
        200,
        &serde_json::to_value(mailbox_core::store::write_result(&written))?,
    ))
}

fn snooze(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let refs = ids(&body)?;

    let mut workspace = writable(session, tracer, clock)?;
    match body.get("until").and_then(|v| v.as_str()) {
        Some(until) => {
            let when = mailbox_core::store::snooze::parse_until(until, clock)?;
            let ids = workspace.snooze(&refs, when)?;
            Ok(Response::json(
                200,
                &serde_json::json!({
                    "snoozed": ids,
                    "until": mailbox_core::clock::format_rfc3339(when),
                    "device": mailbox_core::paths::device_name(),
                }),
            ))
        }
        // No time means wake it, which is what the picker's "unsnooze" does.
        None => Ok(Response::json(
            200,
            &serde_json::json!({ "woke": workspace.unsnooze(&refs)? }),
        )),
    }
}

/// Fetches one message's body now, rather than when a sync next gets to it.
///
/// §5.1 fetches bodies lazily, so a message opened from outside the prefetch
/// window has headers and no body — a normal state, and one the page used to
/// report as though it were permanent. This is the "fetch it then" the
/// reader's note now offers, and it is the same call the terminal makes when
/// a message is opened.
fn fetch_body(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Result<Response> {
    let body = body(request)?;
    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::usage("give an `id` to fetch"))?;

    let mut workspace = writable(session, tracer, clock)?;
    // Routed to the account that owns it, like every other write.
    let (account, resolved) = workspace.locate(&IdRef::parse(id))?;
    let downloaded = workspace.sessions_mut()[account].ensure_body(&resolved)?;
    Ok(Response::json(
        200,
        &serde_json::json!({ "downloaded": downloaded }),
    ))
}

fn sync(
    request: &Request,
    workspace: &Mutex<Workspace>,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    progress: &SyncProgress,
) -> Result<Response> {
    let body = if request.body.is_empty() {
        serde_json::Value::Null
    } else {
        body(request)?
    };
    let opts = mailbox_core::imap::sync::SyncOptions {
        full: body.get("full").and_then(|v| v.as_bool()).unwrap_or(false),
        ..Default::default()
    };

    let mut opts_session = session.clone();
    opts_session.read_only = false;
    let mut writable = Workspace::open(&opts_session, tracer.clone(), clock.clone())?;
    let result = {
        let _running = Running(progress);
        writable.sync_watched(&opts, &mut |stage| progress.set(stage))?
    };
    // The reading handle has an older view of the store until it reopens; the
    // caller asked for a sync and then reads, so refresh it now rather than
    // leaving the next `ls` to show what was there before.
    *workspace.lock().unwrap() = Workspace::open(session, tracer.clone(), clock.clone())?;
    Ok(Response::json(200, &serde_json::to_value(result)?))
}

/// What the built-in provider table knows about an address (§9).
///
/// The setup form asks as the address is typed, so that the common case —
/// gmail, outlook, fastmail — needs a password and nothing else, and the host
/// and port fields stay out of the way until they are actually needed. Same
/// call the iOS setup screen makes, and the same answer.
///
/// It reads nothing and writes nothing: the table is compiled in, and an
/// address is not a secret this server did not already have.
fn providers(request: &Request) -> Result<Response> {
    let address = request
        .query
        .get("address")
        .ok_or_else(|| Error::usage("providers needs an `address`"))?;

    let payload = match mailbox_core::providers::hints_for(address) {
        Some(h) => serde_json::json!({
            "known": true,
            "imap_host": h.imap_host,
            "imap_port": h.imap_port,
            "imap_security": h.imap_security,
            "smtp_host": h.smtp_host,
            "smtp_port": h.smtp_port,
            "smtp_security": h.smtp_security,
            // Said in the same shape `discover` says it, so the form has one
            // way of explaining where a setting came from. A hit here is the
            // built-in table by definition, and the most reassuring of the
            // four answers.
            "imap_source": mailbox_core::providers::Source::Table.as_str(),
            "smtp_source": mailbox_core::providers::Source::Table.as_str(),
            "authoritative": true,
        }),
        // Not an error: an address nobody has a table entry for is an ordinary
        // account that needs its servers typed in, which is what the form does
        // with this answer.
        None => serde_json::json!({ "known": false }),
    };
    Ok(Response::json(200, &payload))
}

/// How long the setup form's login attempt may take.
///
/// Long enough for a slow provider and a TLS handshake, short enough that a
/// host that is not answering is reported as such rather than leaving someone
/// watching a spinner. One connection thread is held for this, and only
/// during setup.
const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The fields the setup form may set.
///
/// Deliberately not [`mailbox_core::accounts::NewAccount`] itself, even though
/// that would deserialize straight from this body. `NewAccount` carries
/// `credential_command` — a shell command run by
/// [`mailbox_core::credentials::resolve`] — and accepting it here would put a
/// "run this command" field on an HTTP route, which is the one thing the top
/// of this file says is not on offer. What a page may set is what a page needs
/// to set: an address, a password, and the servers to reach.
#[derive(serde::Deserialize)]
struct SetupForm {
    address: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    imap_host: Option<String>,
    #[serde(default)]
    imap_port: Option<u16>,
    #[serde(default)]
    imap_user: Option<String>,
    /// Carried from whatever filled the form in rather than typed: the two
    /// lookup routes report a security for each host, and dropping it would
    /// silently downgrade a STARTTLS server to the implicit-TLS default.
    /// `validate` in the core is what stops it being abused — the values are a
    /// closed set, and turning TLS off is refused for anything but loopback.
    #[serde(default)]
    imap_security: Option<String>,
    #[serde(default)]
    smtp_host: Option<String>,
    #[serde(default)]
    smtp_port: Option<u16>,
    #[serde(default)]
    smtp_user: Option<String>,
    #[serde(default)]
    smtp_security: Option<String>,
}

impl SetupForm {
    fn into_spec(self) -> mailbox_core::accounts::NewAccount {
        // An empty string from an untouched form field is "not given", not a
        // hostname of "". Left as `None` so the provider table still fills it.
        let some = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        mailbox_core::accounts::NewAccount {
            id: self.id,
            address: self.address,
            imap_host: some(self.imap_host),
            imap_port: self.imap_port,
            imap_user: some(self.imap_user),
            imap_security: some(self.imap_security),
            smtp_host: some(self.smtp_host),
            smtp_port: self.smtp_port,
            smtp_user: some(self.smtp_user),
            smtp_security: some(self.smtp_security),
            password: some(self.password),
            ..Default::default()
        }
    }
}

/// The same question, asked of the network (§9's autoconfig).
///
/// Behind a button rather than run as the address is typed, because it blocks
/// on DNS and because it tells a domain's nameserver that someone here is
/// setting up mail for it. The answer says where each setting came from, so
/// the form can present a `SRV` record as found and a conventional name as a
/// guess — which is the difference between "this is your server" and "this is
/// what your server is usually called".
fn discover(request: &Request) -> Result<Response> {
    let address = request
        .query
        .get("address")
        .ok_or_else(|| Error::usage("discover needs an `address`"))?;

    let d = mailbox_core::providers::discover(address)?;
    Ok(Response::json(
        200,
        &serde_json::json!({
            "known": true,
            "imap_host": d.hints.imap_host,
            "imap_port": d.hints.imap_port,
            "imap_security": d.hints.imap_security,
            "smtp_host": d.hints.smtp_host,
            "smtp_port": d.hints.smtp_port,
            "smtp_security": d.hints.smtp_security,
            "imap_source": d.imap_source.as_str(),
            "smtp_source": d.smtp_source.as_str(),
            // One flag, because the question a setup screen asks is "may I
            // present this as found, or must I say it is a guess?".
            "authoritative": d.imap_source.is_authoritative() && d.smtp_source.is_authoritative(),
        }),
    ))
}

/// Tries an account's settings without adding it — the Test button (§9).
///
/// `mailbox accounts test`, made available before there is an account to name.
/// The CLI's version tests one that already exists, because on a terminal the
/// order does not matter: a wrong setting is an `accounts edit` away. On a
/// page it does matter, so this comes first and answers with the same things
/// the CLI prints — what the server offers, how many folders are there, and
/// which of §5.3's fast paths a sync will take.
///
/// Writes nothing, anywhere. Refused off loopback for the same reason `add`
/// is: it carries a password.
fn test_account(request: &Request, tracer: &Tracer, setup_allowed: bool) -> Result<Response> {
    if !setup_allowed {
        return Err(off_loopback());
    }

    let form: SetupForm = serde_json::from_slice(&request.body)
        .map_err(|e| Error::usage(format!("the request body is not an account: {e}")))?;
    let verified = try_settings(&form.into_spec(), tracer)?;
    Ok(Response::json(200, &serde_json::to_value(verified)?))
}

/// Connects and logs in with what the form holds, and says what it found.
///
/// Shared by Test and Add so the two cannot disagree: an Add that succeeded
/// where a Test had just failed — or the reverse — would make the button a
/// lie.
fn try_settings(
    spec: &mailbox_core::accounts::NewAccount,
    tracer: &Tracer,
) -> Result<mailbox_core::accounts::Verified> {
    let (id, cfg, _) = mailbox_core::accounts::prepare(spec)?;
    let password = spec
        .password
        .clone()
        .filter(|p| !p.is_empty())
        // The CLI will happily add an account and leave the credential for
        // later, because `credential_command` and `MAILBOX_PASSWORD_*` are
        // there for exactly that. Neither is reachable from a page, so an
        // account added here without a password is one that can never work.
        .ok_or_else(|| Error::usage("an account added from here needs a password"))?;

    mailbox_core::accounts::verify(
        &id,
        &cfg,
        &mailbox_core::imap::Credential::Password(password),
        tracer.clone(),
        VERIFY_TIMEOUT,
    )
}

fn config_path(session: &SessionOptions) -> std::path::PathBuf {
    session
        .config_path
        .clone()
        .unwrap_or_else(mailbox_core::paths::config_file)
}

/// The interface's preferences, from `[gui]` in the config.
fn settings(session: &SessionOptions) -> Result<Response> {
    let config = mailbox_core::config::Config::load(&config_path(session))?;
    Ok(Response::json(200, &serde_json::to_value(config.gui)?))
}

/// Changes the fields sent and leaves the rest of the config as it was.
fn save_settings(request: &Request, session: &SessionOptions) -> Result<Response> {
    let changes = body(request)?;
    let path = config_path(session);
    let mut config = mailbox_core::config::Config::load(&path)?;
    for (key, slot) in [
        ("swipe_left", &mut config.gui.swipe_left),
        ("swipe_right", &mut config.gui.swipe_right),
    ] {
        let Some(value) = changes.get(key) else {
            continue;
        };
        let value = value.as_str().unwrap_or_default();
        if !mailbox_core::config::SWIPE_ACTIONS.contains(&value) {
            return Err(Error::usage(format!(
                "`{value}` is not something a swipe can do; one of {}",
                mailbox_core::config::SWIPE_ACTIONS.join(", ")
            )));
        }
        *slot = value.to_string();
    }
    config.save(&path)?;
    Ok(Response::json(200, &serde_json::to_value(config.gui)?))
}

/// Why this server will not take a password.
fn off_loopback() -> Error {
    Error::usage(
        "this server is not on loopback, and a password typed into it would cross the \
         network in the clear. Add the account from `mailbox serve` on the machine \
         itself, or from `mailbox accounts add` (§9).",
    )
}

/// Adds an account (§9), from the page rather than from a terminal.
///
/// The one route here that writes the config. It answers with what
/// `mailbox accounts add --json` answers with, because §4 allows exactly one
/// contract and this is a second frontend for the same operation, not a
/// second operation.
///
/// The password does not come back, is not written to the config, and is not
/// traced: it goes to [`mailbox_core::accounts::add`], which hands it to the
/// keychain and to this process's memory (§9).
fn add_account(
    request: &Request,
    workspace: &Mutex<Workspace>,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    setup_allowed: bool,
) -> Result<Response> {
    if !setup_allowed {
        return Err(off_loopback());
    }

    let form: SetupForm = serde_json::from_slice(&request.body)
        .map_err(|e| Error::usage(format!("the request body is not an account: {e}")))?;
    let spec = form.into_spec();

    // Tried before it is written, which is the whole difference between this
    // and the CLI's `accounts add`. There, a password that turns out to be
    // wrong is fixed with `accounts edit`; here the person who filled the form
    // in may have no terminal to run that from, so an account that does not
    // work must never come into existence in the first place. The cost is one
    // IMAP login, on the request — the same one the Test button spends.
    try_settings(&spec, tracer)?;

    let path = session
        .config_path
        .clone()
        .unwrap_or_else(mailbox_core::paths::config_file);
    let added = mailbox_core::accounts::add(&path, &spec)?;

    // The reading handle was opened against a config without this account in
    // it. Reopened here rather than left to the next request, so the listing
    // the page loads immediately afterwards is the new account's and not an
    // empty one.
    if let Ok(reopened) = Workspace::open_or_empty(session, tracer.clone(), clock.clone()) {
        *workspace.lock().unwrap() = reopened;
    }

    Ok(Response::json(200, &serde_json::to_value(added)?))
}

fn edit_account(
    request: &Request,
    workspace: &Mutex<Workspace>,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
    setup_allowed: bool,
) -> Result<Response> {
    if !setup_allowed {
        return Err(off_loopback());
    }
    let form: SetupForm = serde_json::from_slice(&request.body)
        .map_err(|e| Error::usage(format!("the request body is not an account: {e}")))?;
    let id = form.id.trim().to_string();
    if id.is_empty() {
        return Err(Error::usage("account edit needs an `id`"));
    }
    let spec = form.into_spec();
    let path = session
        .config_path
        .clone()
        .unwrap_or_else(mailbox_core::paths::config_file);
    mailbox_core::accounts::edit(&path, &id, &spec)?;
    *workspace.lock().unwrap() = Workspace::open_or_empty(session, tracer.clone(), clock.clone())?;
    Ok(Response::json(200, &serde_json::json!({"updated": id})))
}

/// A message body, as a document of its own, for the interface to frame.
///
/// Its own URL rather than a `srcdoc`, and that is the whole point. A `srcdoc`
/// frame inherits the embedding page's content policy, so the *page's*
/// `img-src` decides whether a message's images can load — which meant the
/// reader could say "load images" and be quietly overruled by a header written
/// for a different purpose. A framed document with a real URL gets exactly the
/// policy sent with it, and the `sandbox` attribute still gives it a unique
/// origin, so nothing here can reach the page around it (§9).
///
/// This is a page rather than an API route, and is authenticated like one: a
/// frame navigation cannot set a header, so it carries the cookie. That is
/// safe here in a way it would not be for a write — no other origin can frame
/// it (`frame-ancestors 'self'`) or read it (no CORS), and it changes nothing.
/// One attachment's bytes, for opening or saving.
///
/// Navigated to rather than fetched, like the body document, so the browser
/// and the desktop can hand the file to whatever opens it.
///
/// Always a download, never a page. The bytes are a stranger's file arriving
/// on this app's own origin, so `Content-Disposition: attachment` and the
/// `nosniff` every response carries are what stop an HTML attachment from
/// being rendered here with the page's privileges — the same reason §9 puts the message body
/// in a sandboxed frame.
pub fn attachment(
    request: &Request,
    session: &SessionOptions,
    tracer: &Tracer,
    clock: &Clock,
) -> Response {
    let (Some(id), Some(part)) = (request.query.get("id"), request.query.get("part")) else {
        return Response::text(400, "give an `id` and a `part`");
    };

    // Writable, because fetching the part may have to fetch the message
    // first — §5.1 keeps bodies out of the store until something wants one.
    let mut workspace = match writable(session, tracer, clock) {
        Ok(workspace) => workspace,
        Err(e) => return Response::text(500, &e.to_string()),
    };
    let (account, resolved) = match workspace.locate(&IdRef::parse(id)) {
        Ok(found) => found,
        Err(e) => return Response::text(404, &e.to_string()),
    };
    let (filename, mime, bytes) =
        match workspace.sessions_mut()[account].attachment(&resolved, part) {
            Ok(found) => found,
            Err(e) => return Response::text(404, &e.to_string()),
        };

    Response::new(200, &mime, bytes)
        .with(
            "Content-Disposition",
            &format!("attachment; filename=\"{}\"", quoted_filename(&filename)),
        )
        // `nosniff` is already on every response (see `http::Response`), which
        // is what keeps the declared type from being second-guessed here.
        .with("Content-Security-Policy", "default-src 'none'; sandbox")
        .with("Cache-Control", "no-store")
}

/// A filename safe to put inside a quoted header parameter.
///
/// A sender chooses this, so it can hold a quote, a newline, or a path — each
/// of which would either break the header or name a file somewhere else.
fn quoted_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '"' | '\\' => '_',
            '/' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', ' ']);
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn body_document(request: &Request, workspace: &Mutex<Workspace>) -> Response {
    let Some(id) = request.query.get("id") else {
        return Response::text(400, "which message?");
    };
    // The reader's decision, arriving as they made it. Off unless asked for.
    let images = request.query.get("images").is_some_and(|v| v == "1");

    let render = RenderOptions {
        format: BodyFormat::Html,
        ..Default::default()
    };
    let shown = match workspace
        .lock()
        .unwrap()
        .show(&IdRef::parse(id), &render, false)
    {
        Ok(shown) => shown,
        Err(e) => {
            return document(
                &format!("<p><em>{}</em></p>", escape(&e.to_string())),
                false,
            )
        }
    };

    let content = shown
        .message
        .and_then(|m| m.body)
        .map(|b| b.content)
        .unwrap_or_else(|| "<p><em>The body has not been downloaded yet.</em></p>".to_string());

    // The sanitizer disarmed every remote reference into `data-blocked-src`
    // (§9). Re-arming them is this one substitution, made where the disarming
    // happened rather than by string surgery in a browser.
    let content = if images {
        content.replace("data-blocked-src=", "src=")
    } else {
        // A placeholder instead of the browser's broken-image icon, which says
        // "this message is damaged" when what happened is that we declined to
        // fetch something. Substituted for the missing `src`, not alongside a
        // real one — an `<img>` with two `src` attributes uses the first, which
        // is how "load images" managed to load nothing.
        content.replace("<img ", &format!("<img src=\"{PLACEHOLDER}\" "))
    };

    document(&content, images)
}

/// A grey dashed rectangle, inline, so nothing is fetched to draw it.
const PLACEHOLDER: &str = "data:image/svg+xml;charset=utf-8,\
    %3Csvg%20xmlns='http://www.w3.org/2000/svg'%20width='48'%20height='32'%3E\
    %3Crect%20width='48'%20height='32'%20rx='3'%20fill='none'%20stroke='%239a9aa0'\
    %20stroke-dasharray='3%202'/%3E%3C/svg%3E";

fn document(content: &str, images: bool) -> Response {
    let img = if images {
        "img-src data: https: http:"
    } else {
        "img-src data:"
    };
    let page = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <style>{STYLE}</style></head><body>{content}</body></html>"
    );

    Response::new(200, "text/html; charset=utf-8", page.into_bytes())
        .framed()
        .with(
            "Content-Security-Policy",
            &format!(
                "default-src 'none'; {img}; style-src 'unsafe-inline'; \
                 frame-ancestors 'self'; base-uri 'none'; form-action 'none'"
            ),
        )
}

/// The reading typography. Inline because the frame has a unique origin and
/// cannot fetch a stylesheet from anywhere, including from us.
const STYLE: &str = "\
:root { color-scheme: light dark; }
body { margin: 1rem 1.1rem; font: 14px/1.6 -apple-system, BlinkMacSystemFont, \
'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; overflow-wrap: anywhere; }
img, table { max-width: 100%; }
img[data-blocked-src] { opacity: .55; }
blockquote { margin: .6em 0 .6em .4em; padding-left: .8em; \
border-left: 2px solid currentColor; opacity: .65; }
pre { white-space: pre-wrap; font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
a { color: #0a66d0; }
@media (prefers-color-scheme: dark) { a { color: #64a6f5; } }
";

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

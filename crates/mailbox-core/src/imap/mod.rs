//! The IMAP client (§5.3): IMAP4rev1/rev2, with `CONDSTORE` and `QRESYNC` where
//! offered so a reconnect is a delta rather than a rescan.
//!
//! Every extension here is *used when advertised, never required*. A server
//! that offers nothing but bare IMAP4rev1 still syncs correctly, just more
//! expensively — which is the difference between "supports Dovecot" and
//! "supports IMAP".

pub mod connection;
pub mod replay;
pub mod sync;
pub mod transcript;
pub mod utf7;
pub mod watch;

use std::collections::BTreeSet;

use imap_proto::{AttributeValue, MailboxDatum, NameAttribute, Response, ResponseCode};

use crate::error::{Error, Result};
use crate::store::FolderRecord;
use crate::trace::Tracer;

pub use connection::{Connection, ConnectionConfig, Credential, Security};

/// One message as the server described it.
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    /// Unix seconds. Zero when the server omitted `INTERNALDATE`, which only
    /// happens on a flags-only fetch.
    pub internaldate: i64,
    pub size: u64,
    pub modseq: Option<u64>,
    /// `BODY.PEEK[HEADER]`.
    pub headers: Option<Vec<u8>>,
    /// `BODY.PEEK[]` — the complete message.
    pub body: Option<Vec<u8>>,
}

/// A folder as the server describes it, before anything is stored.
#[derive(Debug, Clone)]
pub struct RemoteFolder {
    pub record: FolderRecord,
    /// `MESSAGES` from `STATUS`. `None` when the server would not say — a
    /// container folder, or one that refused.
    pub total: Option<u32>,
    /// `UNSEEN` from `STATUS`.
    pub unread: Option<u32>,
}

impl RemoteFolder {
    fn new(record: FolderRecord, counts: Option<(u32, u32)>) -> RemoteFolder {
        RemoteFolder {
            record,
            total: counts.map(|c| c.0),
            unread: counts.map(|c| c.1),
        }
    }
}

/// Our flag spelling back to IMAP's.
///
/// The inverse of [`crate::flags::normalize`], and deliberately conservative:
/// only the five system flags get a backslash. Everything else goes out as a
/// keyword, because inventing a `\Something` the server never advertised is how
/// a `STORE` gets rejected.
pub fn wire_flag(flag: &str) -> String {
    match flag {
        crate::flags::SEEN => "\\Seen".into(),
        crate::flags::FLAGGED => "\\Flagged".into(),
        crate::flags::ANSWERED => "\\Answered".into(),
        crate::flags::DRAFT => "\\Draft".into(),
        crate::flags::DELETED => "\\Deleted".into(),
        crate::flags::SPAM => "$Junk".into(),
        other => other.to_string(),
    }
}

/// The destination UID from a `UIDPLUS` `[COPYUID …]`.
fn copy_uid(result: &connection::CommandResult) -> Option<u32> {
    match &result.code {
        Some(imap_proto::ResponseCode::CopyUid(_, _, dest)) => dest.first().map(|m| match m {
            imap_proto::types::UidSetMember::Uid(u) => *u,
            imap_proto::types::UidSetMember::UidRange(r) => *r.start(),
        }),
        _ => None,
    }
}

fn status_counts(attrs: &[imap_proto::StatusAttribute]) -> (u32, u32) {
    let mut total = 0;
    let mut unread = 0;
    for a in attrs {
        match a {
            imap_proto::StatusAttribute::Messages(n) => total = *n,
            imap_proto::StatusAttribute::Unseen(n) => unread = *n,
            _ => {}
        }
    }
    (total, unread)
}

/// What a `SELECT`/`EXAMINE` told us.
#[derive(Debug, Clone, Default)]
pub struct Selected {
    pub path: String,
    pub uidvalidity: u32,
    pub uidnext: u32,
    pub exists: u32,
    pub highestmodseq: u64,
    /// UIDs the server reported gone, from `QRESYNC`'s `VANISHED (EARLIER)`.
    pub vanished: Vec<u32>,
    /// Changed messages `QRESYNC` volunteered as part of the select.
    pub changed: Vec<Fetched>,
}

pub struct Client {
    conn: Connection,
    /// The folder currently selected, so a redundant `EXAMINE` can be skipped —
    /// it costs a round trip and, on some servers, a full flag resend.
    current: Option<String>,
    /// Whether the server took `ENABLE UTF8=ACCEPT`, which decides whether
    /// folder names go out as UTF-8 or modified UTF-7.
    utf8_accepted: bool,
}

impl Client {
    pub fn connect(cfg: &ConnectionConfig, tracer: Tracer) -> Result<Client> {
        Ok(Client {
            conn: Connection::connect(cfg, tracer)?,
            current: None,
            utf8_accepted: false,
        })
    }

    /// Replays a recorded conversation instead of connecting (§14.2's L1).
    ///
    /// Returns the client and a handle to the first place the client's
    /// behaviour diverged from the recording, if it did.
    pub fn replay(
        transcript: transcript::Transcript,
        tracer: Tracer,
    ) -> Result<(Client, std::sync::Arc<std::sync::Mutex<Option<String>>>)> {
        let (conn, divergence) = Connection::replay(transcript, tracer)?;
        let utf8_accepted = conn.has_capability("UTF8=ACCEPT");
        Ok((
            Client {
                conn,
                current: None,
                utf8_accepted,
            },
            divergence,
        ))
    }

    /// Notes the sync options of the pass being recorded, for L1.
    pub fn record_sync_options(&self, opts: &sync::SyncOptions) {
        self.conn.record_sync_options(opts);
    }

    /// The conversation recorded so far, when `MAILBOX_RECORD` armed one.
    pub fn transcript(&self) -> Option<transcript::Transcript> {
        self.conn.transcript()
    }

    pub fn login(&mut self, user: &str, credential: &Credential) -> Result<()> {
        self.conn.authenticate(user, credential)?;

        // Order matters: UTF8=ACCEPT changes how mailbox names are encoded for
        // the rest of the session, so it is enabled before anything lists them.
        let mut wanted: Vec<&str> = Vec::new();
        if self.conn.has_capability("UTF8=ACCEPT") {
            wanted.push("UTF8=ACCEPT");
        }
        // QRESYNC implies CONDSTORE, so asking for both is redundant and some
        // servers complain about it.
        if self.conn.has_capability("QRESYNC") {
            wanted.push("QRESYNC");
        } else if self.conn.has_capability("CONDSTORE") {
            wanted.push("CONDSTORE");
        }
        self.conn.enable(&wanted)?;
        self.utf8_accepted = self.conn.has_capability("UTF8=ACCEPT");
        // From here on the conversation is the sync itself, which is what an L1
        // transcript is a recording of (§14.2).
        self.conn.begin_recording();
        Ok(())
    }

    pub fn capabilities(&self) -> &BTreeSet<String> {
        &self.conn.capabilities
    }

    pub fn has(&self, cap: &str) -> bool {
        self.conn.has_capability(cap)
    }

    pub fn qresync(&self) -> bool {
        self.conn.qresync_enabled
    }

    pub fn logout(&mut self) -> Result<()> {
        self.conn.logout()
    }

    /// Waits for the server to speak, or for `window` (§5.3's `IDLE`).
    pub fn idle(&mut self, window: std::time::Duration) -> Result<crate::imap::connection::Idle> {
        self.conn.idle(window)
    }

    /// Lists every folder, with `SPECIAL-USE` where the server offers it.
    pub fn list_folders(&mut self) -> Result<Vec<FolderRecord>> {
        // LIST-EXTENDED lets one command return special-use annotations; without
        // it we fall back to guessing from the name, which is what every client
        // did before RFC 6154 and is still right most of the time.
        let cmd = if self.has("LIST-EXTENDED") && self.has("SPECIAL-USE") {
            "LIST \"\" \"*\" RETURN (SPECIAL-USE)"
        } else {
            "LIST \"\" \"*\""
        };
        let result = self.conn.command(cmd)?.require_ok("LIST")?;

        let mut out = Vec::new();
        for r in &result.responses {
            let Response::MailboxData(MailboxDatum::List {
                name_attributes,
                delimiter,
                name,
            }) = r
            else {
                continue;
            };
            out.push(self.folder_record(name_attributes, delimiter, name));
        }
        Ok(out)
    }

    /// Changes an existing mailbox's SPECIAL-USE metadata (RFC 6154 §4).
    pub fn set_special_use(&mut self, path: &str, use_name: Option<&str>) -> Result<()> {
        if self.has("SPECIAL-USE") && !self.has("METADATA") {
            return Err(Error::usage(
                "this server advertises SPECIAL-USE for listing, but not METADATA for editing; \
                 Dovecot normally sets these roles in its namespace mailbox `special_use` \
                 configuration — change and reload the server configuration, then run `mailbox sync`",
            ));
        }
        if !self.has("METADATA") || !self.has("SPECIAL-USE") {
            return Err(Error::usage(
                "this IMAP server does not support editing SPECIAL-USE metadata",
            ));
        }
        let value = match use_name {
            None => "NIL".to_string(),
            Some(name) => {
                let attribute = match name {
                    "all" => "\\All",
                    "archive" => "\\Archive",
                    "drafts" => "\\Drafts",
                    "flagged" => "\\Flagged",
                    "important" => "\\Important",
                    "junk" => "\\Junk",
                    "sent" => "\\Sent",
                    "trash" => "\\Trash",
                    other => return Err(Error::usage(format!("unknown folder use `{other}`"))),
                };
                connection::quote_astring(attribute)
            }
        };
        let command = format!(
            "SETMETADATA {} (/private/specialuse {value})",
            self.encode_mailbox(path)
        );
        match self.conn.command(&command)?.require_ok("SETMETADATA") {
            Ok(_) => Ok(()),
            Err(Error::Rejected { message, .. })
                if message.to_ascii_lowercase().contains("specialuse")
                    && message.to_ascii_lowercase().contains("cannot be changed") =>
            {
                Err(Error::usage(
                    "this server exposes SPECIAL-USE metadata as read-only; Dovecot normally \
                     controls it with namespace mailbox `special_use` configuration — change \
                     and reload the server configuration, then run `mailbox sync`. To choose \
                     among folders that already have the same role, use `mailbox folders prefer`",
                ))
            }
            Err(error) => Err(error),
        }
    }

    /// Turns one `LIST` response into a folder record: the name decoded from
    /// modified UTF-7, the leaf split on the server's own delimiter, and the
    /// special-use resolved.
    fn folder_record(
        &self,
        name_attributes: &[NameAttribute<'static>],
        delimiter: &Option<std::borrow::Cow<'static, str>>,
        name: &str,
    ) -> FolderRecord {
        let path = if self.utf8_accepted {
            name.to_string()
        } else {
            utf7::decode(name)
        };
        let delim = delimiter.as_ref().map(|d| d.to_string());
        let leaf = match &delim {
            Some(d) if !d.is_empty() => path.rsplit(d.as_str()).next().unwrap_or(&path).to_string(),
            _ => path.clone(),
        };
        FolderRecord {
            special_use: special_use(name_attributes, &path, self.has("SPECIAL-USE")),
            selectable: is_selectable(name_attributes),
            path,
            name: leaf,
            delimiter: delim,
            subscribed: true,
        }
    }

    /// Lists every folder *with its message counts*, without selecting any of
    /// them and without touching the local store.
    ///
    /// This is what answers "what is on the server, and how big is it" before
    /// committing to a sync. `LIST-STATUS` (RFC 5819) folds the counts into the
    /// one `LIST` round trip where the server offers it; where it does not, a
    /// `STATUS` per folder is the fallback — more round trips, but `STATUS` is
    /// IMAP4rev1 core, so the counts are always obtainable.
    pub fn list_folders_with_counts(&mut self) -> Result<Vec<RemoteFolder>> {
        if self.has("LIST-STATUS") {
            return self.list_status();
        }

        let mut out = Vec::new();
        for record in self.list_folders()? {
            // `STATUS` on a container folder is an error on most servers, and
            // the honest count for something that cannot hold mail is zero.
            let counts = if record.selectable {
                self.status(&record.path)?
            } else {
                None
            };
            out.push(RemoteFolder::new(record, counts));
        }
        Ok(out)
    }

    fn list_status(&mut self) -> Result<Vec<RemoteFolder>> {
        let special_use = if self.has("SPECIAL-USE") {
            "SPECIAL-USE "
        } else {
            ""
        };
        let result = self
            .conn
            .command(&format!(
                "LIST \"\" \"*\" RETURN ({special_use}STATUS (MESSAGES UNSEEN))"
            ))?
            .require_ok("LIST")?;

        // The two response kinds interleave, and the STATUS names arrive in the
        // server's own encoding — so they are matched on the wire name before
        // anything is decoded.
        let mut counts: std::collections::HashMap<String, (u32, u32)> = Default::default();
        let mut records = Vec::new();
        for r in &result.responses {
            match r {
                Response::MailboxData(MailboxDatum::Status { mailbox, status }) => {
                    counts.insert(mailbox.to_string(), status_counts(status));
                }
                Response::MailboxData(MailboxDatum::List {
                    name_attributes,
                    delimiter,
                    name,
                }) => {
                    records.push((
                        name.to_string(),
                        self.folder_record(name_attributes, delimiter, name),
                    ));
                }
                _ => {}
            }
        }

        Ok(records
            .into_iter()
            .map(|(wire_name, record)| {
                let counts = counts.get(&wire_name).copied();
                RemoteFolder::new(record, counts)
            })
            .collect())
    }

    /// `STATUS` for one folder: message and unseen counts, without selecting it.
    pub fn status(&mut self, path: &str) -> Result<Option<(u32, u32)>> {
        let encoded = self.encode_mailbox(path);
        let result = self
            .conn
            .command(&format!("STATUS {encoded} (MESSAGES UNSEEN)"))?;
        if !result.ok() {
            // A folder that refuses STATUS is not a failure of the listing —
            // report the folder without counts rather than losing the row.
            return Ok(None);
        }
        for r in &result.responses {
            if let Response::MailboxData(MailboxDatum::Status { status, .. }) = r {
                return Ok(Some(status_counts(status)));
            }
        }
        Ok(None)
    }

    /// Opens a folder read-only.
    ///
    /// `EXAMINE` rather than `SELECT` throughout M1: a read sync must never set
    /// `\Seen` as a side effect of looking at a message, and `EXAMINE` makes
    /// that a property of the protocol rather than of our discipline.
    /// `qresync` is `(uidvalidity, highestmodseq)` from the last pass.
    pub fn examine(&mut self, path: &str, qresync: Option<(u32, u64)>) -> Result<Selected> {
        let encoded = self.encode_mailbox(path);
        let cmd = match qresync {
            Some((uidvalidity, modseq)) if self.conn.qresync_enabled && modseq > 0 => {
                format!("EXAMINE {encoded} (QRESYNC ({uidvalidity} {modseq}))")
            }
            _ => format!("EXAMINE {encoded}"),
        };
        let result = self.conn.command(&cmd)?;
        if !result.ok() {
            self.current = None;
            return Err(Error::not_found("folder", path));
        }
        self.current = Some(path.to_string());

        Ok(self.selected_from(path, &result))
    }

    /// Reads a `SELECT`/`EXAMINE` completion into [`Selected`].
    ///
    /// Shared by both, because the difference between them is write access, not
    /// what the server says about the mailbox.
    fn selected_from(&self, path: &str, result: &connection::CommandResult) -> Selected {
        let mut sel = Selected {
            path: path.to_string(),
            ..Default::default()
        };
        if let Some(code) = &result.code {
            apply_code(&mut sel, code);
        }
        for r in &result.responses {
            match r {
                Response::Data {
                    code: Some(code), ..
                } => apply_code(&mut sel, code),
                Response::MailboxData(MailboxDatum::Exists(n)) => sel.exists = *n,
                Response::Vanished { uids, .. } => {
                    for range in uids {
                        sel.vanished.extend(range.clone());
                    }
                }
                Response::Fetch(_, attrs) => {
                    if let Some(f) = fetched_from(attrs) {
                        sel.changed.push(f);
                    }
                }
                _ => {}
            }
        }
        sel
    }

    /// Opens a folder for writing.
    ///
    /// The read path uses `EXAMINE` so a sync can never set `\Seen` as a side
    /// effect (§5.3). Pushing a queued write needs `SELECT`, and this is the
    /// only place that asks for it.
    pub fn select(&mut self, path: &str) -> Result<Selected> {
        let encoded = self.encode_mailbox(path);
        let result = self.conn.command(&format!("SELECT {encoded}"))?;
        if !result.ok() {
            self.current = None;
            return Err(Error::not_found("folder", path));
        }
        self.current = Some(path.to_string());
        Ok(self.selected_from(path, &result))
    }

    /// `UID STORE` — adds and removes flags in one round trip where both are
    /// needed.
    ///
    /// Returns false when the server refused, so the caller can distinguish
    /// "the message is gone" from "the connection broke".
    pub fn store_flags(&mut self, uid: u32, add: &[String], remove: &[String]) -> Result<()> {
        self.require_selected()?;
        for (op, list) in [("+", add), ("-", remove)] {
            if list.is_empty() {
                continue;
            }
            let wire: Vec<String> = list.iter().map(|f| wire_flag(f)).collect();
            self.conn
                .command(&format!(
                    "UID STORE {uid} {op}FLAGS.SILENT ({})",
                    wire.join(" ")
                ))?
                .require_ok("STORE")?;
        }
        Ok(())
    }

    /// Moves a message to another folder.
    ///
    /// `MOVE` (RFC 6851) where advertised; `COPY` + `\Deleted` + `UID EXPUNGE`
    /// otherwise. The fallback is why `UIDPLUS` matters: a plain `EXPUNGE`
    /// removes *every* `\Deleted` message in the folder, including ones another
    /// client marked and did not intend us to remove.
    ///
    /// Returns the UID the message was given in the destination, when the
    /// server said (`UIDPLUS`).
    pub fn move_message(&mut self, uid: u32, target: &str) -> Result<Option<u32>> {
        self.require_selected()?;
        let encoded = self.encode_mailbox(target);

        if self.has("MOVE") {
            let result = self.conn.command(&format!("UID MOVE {uid} {encoded}"))?;
            let result = result.require_ok("MOVE")?;
            return Ok(copy_uid(&result));
        }

        let result = self.conn.command(&format!("UID COPY {uid} {encoded}"))?;
        let result = result.require_ok("COPY")?;
        let new_uid = copy_uid(&result);

        self.conn
            .command(&format!("UID STORE {uid} +FLAGS.SILENT (\\Deleted)"))?
            .require_ok("STORE")?;
        self.expunge_uid(uid)?;
        Ok(new_uid)
    }

    /// Copies a message, leaving the original where it is.
    pub fn copy_message(&mut self, uid: u32, target: &str) -> Result<Option<u32>> {
        self.require_selected()?;
        let encoded = self.encode_mailbox(target);
        let result = self.conn.command(&format!("UID COPY {uid} {encoded}"))?;
        Ok(copy_uid(&result.require_ok("COPY")?))
    }

    /// Removes one message: `\Deleted` then expunge.
    pub fn delete_message(&mut self, uid: u32) -> Result<()> {
        self.require_selected()?;
        self.conn
            .command(&format!("UID STORE {uid} +FLAGS.SILENT (\\Deleted)"))?
            .require_ok("STORE")?;
        self.expunge_uid(uid)
    }

    /// Expunges exactly one UID where `UIDPLUS` allows it, and declines to
    /// expunge at all where it does not.
    ///
    /// A bare `EXPUNGE` would remove every `\Deleted` message in the folder,
    /// including ones another client marked for its own reasons. Leaving the
    /// message flagged `\Deleted` but present is a visible, recoverable state;
    /// deleting somebody else's mail is not.
    fn expunge_uid(&mut self, uid: u32) -> Result<()> {
        if self.has("UIDPLUS") {
            self.conn
                .command(&format!("UID EXPUNGE {uid}"))?
                .require_ok("UID EXPUNGE")?;
        }
        Ok(())
    }

    /// `UID SEARCH`. `criteria` is raw IMAP search syntax, which is also how
    /// `raw:` from §6.2 reaches the server.
    pub fn uid_search(&mut self, criteria: &str) -> Result<Vec<u32>> {
        self.require_selected()?;
        let result = self
            .conn
            .command(&format!("UID SEARCH {criteria}"))?
            .require_ok("SEARCH")?;
        let mut uids = Vec::new();
        for r in &result.responses {
            if let Response::MailboxData(MailboxDatum::Search(list)) = r {
                uids.extend(list.iter().copied());
            }
        }
        uids.sort_unstable();
        uids.dedup();
        Ok(uids)
    }

    /// Finds a message by its `Message-ID` header in the selected folder.
    ///
    /// The fallback when a queued operation's recorded UID is no longer
    /// trustworthy — after a `UIDVALIDITY` reset, or when the message was
    /// already moved by another client. `HEADER` search is exact and works on
    /// every server; it is a round trip, which is why it is a fallback.
    pub fn find_by_message_id(&mut self, message_id: &str) -> Result<Option<u32>> {
        let bare = message_id.trim_start_matches('<').trim_end_matches('>');
        let uids = self.uid_search(&format!(
            "HEADER MESSAGE-ID {}",
            connection::quote_astring(&format!("<{bare}>"))
        ))?;
        // More than one match means two messages share a Message-ID, which
        // happens. The lowest UID is the oldest, and picking deterministically
        // beats picking arbitrarily.
        Ok(uids.first().copied())
    }

    /// Headers, flags, and structure for a set of UIDs — the §5.1 default
    /// fetch, which is everything except the body.
    pub fn fetch_headers(&mut self, uids: &[u32]) -> Result<Vec<Fetched>> {
        self.fetch(
            uids,
            "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER])",
        )
    }

    /// The complete message, for a body the user asked for or the prefetch
    /// window covers.
    pub fn fetch_bodies(&mut self, uids: &[u32]) -> Result<Vec<Fetched>> {
        self.fetch(uids, "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])")
    }

    /// Flags only, for the cheap "what changed" pass.
    pub fn fetch_flags(&mut self, uids: &[u32]) -> Result<Vec<Fetched>> {
        self.fetch(uids, "(UID FLAGS)")
    }

    /// `CONDSTORE`'s delta: everything whose `MODSEQ` exceeds `since`.
    ///
    /// This is the whole point of the extension — the alternative is fetching
    /// flags for every message in the folder on every pass.
    pub fn fetch_changed_since(&mut self, since: u64) -> Result<Vec<Fetched>> {
        self.require_selected()?;
        let result = self
            .conn
            .command(&format!(
                "UID FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE {since})"
            ))?
            .require_ok("FETCH")?;
        Ok(collect_fetched(&result.responses))
    }

    fn fetch(&mut self, uids: &[u32], items: &str) -> Result<Vec<Fetched>> {
        self.require_selected()?;
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(uids.len());
        // Chunked so one command's response cannot grow without bound, and so a
        // dropped connection mid-sync loses one chunk rather than the pass.
        for chunk in uids.chunks(FETCH_CHUNK) {
            let result = self
                .conn
                .command(&format!("UID FETCH {} {items}", uid_set(chunk)))?
                .require_ok("FETCH")?;
            out.extend(collect_fetched(&result.responses));
        }
        Ok(out)
    }

    /// Creates a folder. Used by the fixture seeder (§14.1) and, from M2, by
    /// the write path.
    ///
    /// An `ALREADYEXISTS` refusal is success: the seeder's job is to make the
    /// folder be there, not to be the one that made it.
    pub fn create_folder(&mut self, path: &str) -> Result<()> {
        let encoded = self.encode_mailbox(path);
        let result = self.conn.command(&format!("CREATE {encoded}"))?;
        if result.ok() {
            return Ok(());
        }
        let info = result
            .information
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if info.contains("exist") {
            return Ok(());
        }
        result.require_ok("CREATE").map(|_| ())
    }

    /// Appends a message to a folder.
    ///
    /// This is how the L2 seeder puts a fixture on a real server (§14.1) — and
    /// why assertions can name `fixture["invoice-with-pdf"]` instead of a UID:
    /// with `UIDPLUS` the server tells us the UID it assigned, and the seeder
    /// records it rather than the test hardcoding one.
    pub fn append(
        &mut self,
        path: &str,
        flags: &[String],
        internaldate: Option<i64>,
        body: &[u8],
    ) -> Result<Option<u32>> {
        let encoded = self.encode_mailbox(path);
        let flag_list = if flags.is_empty() {
            String::new()
        } else {
            format!("({}) ", flags.join(" "))
        };
        let date = match internaldate {
            Some(ts) => format!("\"{}\" ", imap_internaldate(ts)),
            None => String::new(),
        };

        let result = self
            .conn
            .append_literal(&format!("APPEND {encoded} {flag_list}{date}"), body)?;
        let result = result.require_ok("APPEND")?;

        // UIDPLUS: `[APPENDUID <uidvalidity> <uid>]`.
        Ok(match result.code {
            Some(imap_proto::ResponseCode::AppendUid(_, ref set)) => set.first().map(|m| match m {
                imap_proto::types::UidSetMember::Uid(u) => *u,
                imap_proto::types::UidSetMember::UidRange(r) => *r.start(),
            }),
            _ => None,
        })
    }

    /// Sends a command verbatim and returns whether it succeeded.
    ///
    /// The escape hatch for the L2 harness (§14.2), which has to *drive* the
    /// server — expunge a message, delete a folder — to create the states the
    /// sync engine must recover from. Not a path the product uses: everything
    /// the client does for real has a typed method above.
    pub fn raw_command(&mut self, cmd: &str) -> Result<()> {
        let result = self.conn.command(cmd)?;
        if cmd
            .split(' ')
            .next()
            .unwrap_or("")
            .eq_ignore_ascii_case("SELECT")
        {
            self.current = Some(cmd.to_string());
        }
        result.require_ok(cmd).map(|_| ())
    }

    fn require_selected(&self) -> Result<()> {
        if self.current.is_none() {
            return Err(Error::protocol("no folder is selected"));
        }
        Ok(())
    }

    fn encode_mailbox(&self, path: &str) -> String {
        let name = if self.utf8_accepted {
            path.to_string()
        } else {
            utf7::encode(path)
        };
        connection::quote_astring(&name)
    }
}

/// How many UIDs go into one `FETCH`. Large enough that the round trips do not
/// dominate, small enough that a body fetch's response stays a sane size.
const FETCH_CHUNK: usize = 200;

fn apply_code(sel: &mut Selected, code: &ResponseCode<'static>) {
    match code {
        ResponseCode::UidValidity(v) => sel.uidvalidity = *v,
        ResponseCode::UidNext(v) => sel.uidnext = *v,
        ResponseCode::HighestModSeq(v) => sel.highestmodseq = *v,
        _ => {}
    }
}

fn collect_fetched(responses: &[Response<'static>]) -> Vec<Fetched> {
    responses
        .iter()
        .filter_map(|r| match r {
            Response::Fetch(_, attrs) => fetched_from(attrs),
            _ => None,
        })
        .collect()
}

/// A `FETCH` without a `UID` cannot be matched to anything we store — sequence
/// numbers shift under us — so it is dropped rather than guessed at.
fn fetched_from(attrs: &[AttributeValue<'static>]) -> Option<Fetched> {
    let mut f = Fetched::default();
    let mut has_uid = false;

    for a in attrs {
        match a {
            AttributeValue::Uid(uid) => {
                f.uid = *uid;
                has_uid = true;
            }
            AttributeValue::Flags(flags) => {
                f.flags = flags.iter().map(|c| c.to_string()).collect();
            }
            AttributeValue::InternalDate(d) => {
                f.internaldate = parse_internaldate(d).unwrap_or(0);
            }
            AttributeValue::Rfc822Size(n) => f.size = u64::from(*n),
            AttributeValue::ModSeq(m) => f.modseq = Some(*m),
            AttributeValue::Rfc822Header(Some(data)) => f.headers = Some(data.to_vec()),
            AttributeValue::Rfc822(Some(data)) => f.body = Some(data.to_vec()),
            // `BODY.PEEK[]` and `BODY.PEEK[HEADER]` both come back as
            // `BodySection`, and the section is the only thing that tells them
            // apart. Getting this wrong is silent: the headers land in `body`,
            // the sync engine finds no header block, and every message is
            // skipped without an error anywhere.
            AttributeValue::BodySection {
                section,
                data: Some(data),
                ..
            } => {
                use imap_proto::types::{MessageSection, SectionPath};
                match section {
                    // `BODY[]` — the whole message.
                    None => f.body = Some(data.to_vec()),
                    Some(SectionPath::Full(MessageSection::Header)) => {
                        f.headers = Some(data.to_vec())
                    }
                    Some(SectionPath::Part(_, Some(MessageSection::Header))) => {
                        f.headers = Some(data.to_vec())
                    }
                    // `BODY[TEXT]` and `BODY[n]` are neither, and M1 never asks
                    // for them; dropping them is better than filing them as a
                    // whole message.
                    _ => {}
                }
            }
            _ => {}
        }
    }
    has_uid.then_some(f)
}

/// IMAP's `INTERNALDATE` format, for `APPEND` — the same grammar
/// [`parse_internaldate`] reads.
pub fn imap_internaldate(ts: i64) -> String {
    let fmt = time::macros::format_description!(
        "[day]-[month repr:short]-[year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
    );
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|d| d.format(fmt).ok())
        .unwrap_or_else(|| "01-Jan-1970 00:00:00 +0000".to_string())
}

/// `01-Aug-2026 10:00:00 +0200` — IMAP's own date format, which is not RFC 2822
/// and not RFC 3339.
pub fn parse_internaldate(s: &str) -> Option<i64> {
    let fmt = time::macros::format_description!(
        "[day padding:none]-[month repr:short]-[year] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
    );
    time::OffsetDateTime::parse(s.trim(), fmt)
        .ok()
        .map(|d| d.unix_timestamp())
}

/// Compresses a UID list into IMAP's range syntax.
///
/// `1,2,3,4,7` becomes `1:4,7`. Without this a full-mailbox fetch would be a
/// command line megabytes long, and some servers cap command length.
pub fn uid_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut out = String::new();
    let mut start = sorted[0];
    let mut prev = sorted[0];
    for &uid in &sorted[1..] {
        if uid == prev + 1 {
            prev = uid;
            continue;
        }
        push_range(&mut out, start, prev);
        start = uid;
        prev = uid;
    }
    push_range(&mut out, start, prev);
    out
}

fn push_range(out: &mut String, start: u32, end: u32) {
    if !out.is_empty() {
        out.push(',');
    }
    if start == end {
        out.push_str(&start.to_string());
    } else {
        out.push_str(&format!("{start}:{end}"));
    }
}

/// Whether a folder can be `SELECT`ed.
///
/// `\Noselect` is the RFC 3501 spelling; `LIST-EXTENDED` servers answer with
/// `\NonExistent` (RFC 5258) for a container that exists only to hold
/// children, and Dovecot does exactly that under `RETURN (SPECIAL-USE)`.
/// Missing the second spelling means trying to `EXAMINE` a folder that is not
/// there — which is not fatal, but is a round trip and an error per pass,
/// forever.
pub fn is_selectable(attrs: &[NameAttribute<'static>]) -> bool {
    !attrs.iter().any(|a| match a {
        NameAttribute::NoSelect => true,
        NameAttribute::Extension(name) => name.eq_ignore_ascii_case("\\NonExistent"),
        _ => false,
    })
}

/// The special-use name for a folder: from the server's annotation where it
/// gave one, from the name where it did not.
fn special_use(
    attrs: &[NameAttribute<'static>],
    path: &str,
    server_is_authoritative: bool,
) -> Option<String> {
    if path.eq_ignore_ascii_case("INBOX") {
        return Some("inbox".to_string());
    }
    for a in attrs {
        let name = match a {
            NameAttribute::All => "all",
            NameAttribute::Archive => "archive",
            NameAttribute::Drafts => "drafts",
            NameAttribute::Flagged => "flagged",
            NameAttribute::Junk => "junk",
            NameAttribute::Sent => "sent",
            NameAttribute::Trash => "trash",
            NameAttribute::Extension(name) if name.eq_ignore_ascii_case("\\Important") => {
                "important"
            }
            _ => continue,
        };
        return Some(name.to_string());
    }
    if server_is_authoritative {
        None
    } else {
        guess_special_use(path)
    }
}

/// The pre-RFC-6154 heuristic. Deliberately conservative: a wrong guess here
/// changes which folder a message is *reported* as living in (§5.3's ranking),
/// and a false `all` would hide half the mailbox behind the wrong label.
pub fn guess_special_use(path: &str) -> Option<String> {
    let leaf = path
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    let full = path.to_ascii_lowercase();
    Some(
        match leaf.as_str() {
            "inbox" => "inbox",
            "sent" | "sent items" | "sent messages" | "sent mail" => "sent",
            "drafts" | "draft" => "drafts",
            "trash" | "deleted items" | "deleted messages" | "bin" => "trash",
            "junk" | "spam" | "junk e-mail" | "bulk mail" => "junk",
            "archive" | "archives" | "all mail" => {
                if full.contains("all mail") {
                    "all"
                } else {
                    "archive"
                }
            }
            _ => return None,
        }
        .to_string(),
    )
}

//! The read path: search, message and thread retrieval, and the canonical dump
//! the convergence oracle compares (§6.7, §14.3).
//!
//! Everything a listing shows comes from `meta.db` alone — no blob is opened,
//! no network is touched — which is what keeps `ls` inside the 100 ms cold-start
//! budget of §10.

use std::collections::HashMap;

use mailbox_api::{
    Account, Address, Attachment, Folder, Header, MessageDetail, MessageSummary, Thread,
};
use rusqlite::{params_from_iter, types::Value, Row};

use crate::clock::format_rfc3339;
use crate::error::{Error, Result};
use crate::flags;
use crate::id;
use crate::mime;
use crate::query::{compile, CompileOptions, Node};
use crate::render::{self, RenderOptions};
use crate::store::Store;

/// How `ls` and `search` order and page their results.
#[derive(Debug, Clone)]
pub struct ListOptions {
    pub limit: usize,
    /// Opaque, from a previous page's `cursor` (§6.4).
    pub cursor: Option<String>,
    pub sort: Sort,
    /// Identities for `to:me`.
    pub identities: Vec<String>,
    /// Whether to count the full match set. Off by default: on a million-row
    /// mailbox the count costs more than the page it decorates.
    pub with_total: bool,
}

impl Default for ListOptions {
    fn default() -> Self {
        ListOptions {
            limit: 50,
            cursor: None,
            sort: Sort::UnreadFirst,
            identities: Vec::new(),
            with_total: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// Newest first. The order for `search` and for any explicit query.
    DateDesc,
    /// Oldest first.
    DateAsc,
    /// `ls`'s default (§6.1): unread first, then newest.
    UnreadFirst,
}

impl Sort {
    fn order_by(self) -> &'static str {
        match self {
            Sort::DateDesc => "m.date DESC, m.id DESC",
            Sort::DateAsc => "m.date ASC, m.id ASC",
            Sort::UnreadFirst => "(instr(m.flags, ' seen ') > 0) ASC, m.date DESC, m.id DESC",
        }
    }

    /// The comparison that resumes after a cursor. Mirrors `order_by`, and the
    /// two must be changed together or pagination silently loses rows.
    fn after_predicate(self) -> &'static str {
        match self {
            Sort::DateDesc => "(m.date, m.id) < (?, ?)",
            Sort::DateAsc => "(m.date, m.id) > (?, ?)",
            Sort::UnreadFirst => "((instr(m.flags, ' seen ') > 0), -m.date, m.id) > (?, -(?), ?)",
        }
    }
}

pub struct Listing {
    pub messages: Vec<MessageSummary>,
    pub cursor: Option<String>,
    pub total: Option<u64>,
}

/// Columns every summary needs, in one place so the two call sites cannot drift.
const SUMMARY_COLS: &str = "m.rowid, m.id, m.thread_id, m.date, m.subject, m.addrs, m.flags, \
     m.size, m.has_attachment, m.preview, COALESCE(f.path, '')";

impl Store {
    /// Runs a parsed query and returns one page.
    pub fn search(&self, node: &Node, opts: &ListOptions) -> Result<Listing> {
        let compiled = compile(
            node,
            &CompileOptions {
                account: self.account.clone(),
                identities: opts.identities.clone(),
            },
        );

        let mut sql = format!(
            "SELECT {SUMMARY_COLS} FROM messages m
             LEFT JOIN folders f ON f.id = m.primary_folder
             WHERE {}",
            compiled.sql
        );
        let mut params: Vec<Value> = compiled.params.clone();

        if let Some(cursor) = &opts.cursor {
            let c = Cursor::decode(cursor)?;
            sql.push_str(&format!(" AND {}", opts.sort.after_predicate()));
            match opts.sort {
                Sort::UnreadFirst => {
                    params.push(Value::Integer(c.lead));
                    params.push(Value::Integer(c.date));
                    params.push(Value::Text(c.id.clone()));
                }
                _ => {
                    params.push(Value::Integer(c.date));
                    params.push(Value::Text(c.id.clone()));
                }
            }
        }

        sql.push_str(&format!(" ORDER BY {} LIMIT ?", opts.sort.order_by()));
        // One extra row, so "is there a next page" is answered without a second
        // query and without a count.
        params.push(Value::Integer(opts.limit as i64 + 1));

        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(params), |r| {
            Ok(summary_from_row(r, &self.account))
        })?;
        let mut messages: Vec<MessageSummary> = rows.filter_map(std::result::Result::ok).collect();

        let has_more = messages.len() > opts.limit;
        messages.truncate(opts.limit);

        let cursor = if has_more {
            messages.last().map(|last| {
                Cursor {
                    lead: i64::from(last.flags.iter().any(|f| f == flags::SEEN)),
                    date: parse_rfc3339_to_unix(&last.date),
                    id: last.id.clone(),
                }
                .encode()
            })
        } else {
            None
        };

        let total = if opts.with_total {
            let count_sql = format!(
                "SELECT count(*) FROM messages m
                 LEFT JOIN folders f ON f.id = m.primary_folder
                 WHERE {}",
                compiled.sql
            );
            let mut st = self.conn.prepare(&count_sql)?;
            Some(st.query_row(params_from_iter(compiled.params), |r| r.get::<_, i64>(0))? as u64)
        } else {
            None
        };

        Ok(Listing {
            messages,
            cursor,
            total,
        })
    }

    /// Resolves an id or an unambiguous prefix to a full id (§5.2).
    ///
    /// Ambiguity is an error rather than a "first match wins", because the
    /// caller that would be hurt most by a silent pick — an agent acting on the
    /// result — is exactly the one that cannot notice.
    pub fn resolve_id(&self, prefix: &str) -> Result<String> {
        if !crate::hex::is_hex(prefix) {
            return Err(Error::usage(format!("`{prefix}` is not a message id")));
        }
        let pattern = format!("{prefix}%");
        let mut st = self
            .conn
            .prepare("SELECT id FROM messages WHERE id LIKE ?1 ORDER BY id LIMIT 2")?;
        let ids: Vec<String> = st
            .query_map([&pattern], |r| r.get::<_, String>(0))?
            .filter_map(std::result::Result::ok)
            .collect();
        match ids.len() {
            0 => Err(Error::not_found("message", prefix)),
            1 => Ok(ids[0].clone()),
            _ => {
                let count: i64 = self.conn.query_row(
                    "SELECT count(*) FROM messages WHERE id LIKE ?1",
                    [&pattern],
                    |r| r.get(0),
                )?;
                Err(Error::Ambiguous {
                    prefix: prefix.to_string(),
                    count: count as usize,
                })
            }
        }
    }

    pub fn summary(&self, message_id: &str) -> Result<MessageSummary> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLS} FROM messages m
             LEFT JOIN folders f ON f.id = m.primary_folder
             WHERE m.id = ?1"
        ))?;
        st.query_row([message_id], |r| Ok(summary_from_row(r, &self.account)))
            .map_err(|_| Error::not_found("message", message_id))
    }

    /// The full form of a message, rendering the body under `render`.
    ///
    /// A message whose body has not been fetched yet (§5.1 fetches lazily) comes
    /// back with `body: None` rather than an error — that is a normal state, and
    /// the header data is still worth returning.
    pub fn message(&self, message_id: &str, render_opts: &RenderOptions) -> Result<MessageDetail> {
        let summary = self.summary(message_id)?;
        let (rowid, blob_id, msgid, irt, refs_json): (
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
        ) = self.conn.query_row(
            "SELECT rowid, blob_id, message_id, in_reply_to, refs FROM messages WHERE id = ?1",
            [message_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;

        let addrs = self.address_groups(rowid)?;
        let raw = match &blob_id {
            Some(b) => self.blobs.get(b).ok(),
            None => None,
        };
        let parsed = raw.as_deref().map(mime::parse);

        let body = parsed
            .as_ref()
            .and_then(|p| render::body(p, raw.as_deref(), render_opts));
        let headers: Vec<Header> = parsed
            .as_ref()
            .map(|p| p.headers.clone())
            .unwrap_or_default();
        let attachments = self.attachments(rowid)?;

        Ok(MessageDetail {
            summary,
            message_id: msgid,
            in_reply_to: irt,
            references: serde_json::from_str(&refs_json).unwrap_or_default(),
            reply_to: addrs.get("reply_to").cloned().unwrap_or_default(),
            bcc: addrs.get("bcc").cloned().unwrap_or_default(),
            headers,
            body,
            attachments,
        })
    }

    /// The raw RFC5322 bytes, for `show --raw` (§13) and `attach --extract`.
    pub fn raw_message(&self, message_id: &str) -> Result<Vec<u8>> {
        let blob_id: Option<String> = self.conn.query_row(
            "SELECT blob_id FROM messages WHERE id = ?1",
            [message_id],
            |r| r.get(0),
        )?;
        match blob_id {
            Some(b) => self.blobs.get(&b),
            None => Err(Error::not_found("message body", message_id)),
        }
    }

    /// Every message in a thread, in tree order.
    pub fn thread(&self, thread_id: &str) -> Result<Thread> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {SUMMARY_COLS} FROM messages m
             LEFT JOIN folders f ON f.id = m.primary_folder
             WHERE m.thread_id = ?1
             ORDER BY m.thread_depth, m.date, m.id"
        ))?;
        let messages: Vec<MessageSummary> = st
            .query_map([thread_id], |r| Ok(summary_from_row(r, &self.account)))?
            .filter_map(std::result::Result::ok)
            .collect();

        if messages.is_empty() {
            return Err(Error::not_found("thread", thread_id));
        }

        let unread = messages
            .iter()
            .filter(|m| !m.flags.iter().any(|f| f == flags::SEEN))
            .count();
        let last_date = messages
            .iter()
            .map(|m| m.date.clone())
            .max()
            .unwrap_or_default();
        // The subject of the root — the thread is named for where it started,
        // not for whatever the latest reply retitled it.
        let subject = messages
            .first()
            .map(|m| m.subject.clone())
            .unwrap_or_default();

        // Participants, most-recent-first, deduplicated by address.
        let mut participants: Vec<Address> = Vec::new();
        for m in messages.iter().rev() {
            for a in m.from.iter().chain(&m.to) {
                if !participants.iter().any(|p| p.address == a.address) {
                    participants.push(a.clone());
                }
            }
        }

        Ok(Thread {
            id: thread_id.to_string(),
            subject,
            message_count: messages.len() as u32,
            unread_count: unread as u32,
            last_date,
            participants,
            messages,
        })
    }

    pub fn folders(&self) -> Result<Vec<Folder>> {
        let mut st = self.conn.prepare(
            "SELECT path, name, special_use, total, unread, selectable FROM folders
             ORDER BY rank, path",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Folder {
                account: self.account.clone(),
                path: r.get(0)?,
                name: r.get(1)?,
                special_use: r.get(2)?,
                total: r.get::<_, i64>(3)? as u32,
                unread: r.get::<_, i64>(4)? as u32,
                selectable: r.get::<_, i64>(5)? != 0,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn message_count(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))?
            as u64)
    }

    pub fn unread_count(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM messages WHERE instr(flags, ' seen ') = 0",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    pub fn folder_count(&self) -> Result<u32> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM folders", [], |r| r.get::<_, i64>(0))?
            as u32)
    }

    /// Address completion from mail traffic (§2, §5.1).
    pub fn contacts(&self, prefix: &str, limit: usize) -> Result<Vec<Address>> {
        let pattern = format!("%{}%", prefix.to_ascii_lowercase());
        let mut st = self.conn.prepare(
            "SELECT addr, name FROM contacts
             WHERE addr LIKE ?1 OR lower(name) LIKE ?1
             ORDER BY sent_count DESC, seen_count DESC, last_seen DESC
             LIMIT ?2",
        )?;
        let rows = st.query_map(rusqlite::params![pattern, limit as i64], |r| {
            Ok(Address {
                name: Some(r.get::<_, String>(1)?).filter(|n| !n.is_empty()),
                address: r.get(0)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    fn address_groups(&self, rowid: i64) -> Result<HashMap<String, Vec<Address>>> {
        let mut st = self.conn.prepare(
            "SELECT field, name, addr FROM addresses WHERE message_rowid = ?1 ORDER BY field, ord",
        )?;
        let mut out: HashMap<String, Vec<Address>> = HashMap::new();
        let rows = st.query_map([rowid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for (field, name, addr) in rows.flatten() {
            out.entry(field).or_default().push(Address {
                name: Some(name).filter(|n| !n.is_empty()),
                address: addr,
            });
        }
        Ok(out)
    }

    fn attachments(&self, rowid: i64) -> Result<Vec<Attachment>> {
        let mut st = self.conn.prepare(
            "SELECT part, filename, mime_type, size, content_id, is_attachment
             FROM attachments WHERE message_rowid = ?1 ORDER BY part",
        )?;
        let rows = st.query_map([rowid], |r| {
            Ok(Attachment {
                part: r.get(0)?,
                filename: r.get(1)?,
                mime_type: r.get(2)?,
                size: r.get::<_, i64>(3)? as u64,
                content_id: r.get(4)?,
                is_attachment: r.get::<_, i64>(5)? != 0,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// `debug store-dump` (§6.7): a canonical, ordered dump of derived state.
    ///
    /// This is the convergence oracle of §14.3, so two properties matter more
    /// than readability: it must be *complete* about derived state — threads,
    /// flags, counts, placements — and it must be *stable*, ordered by
    /// something no server assigns. Everything here sorts on the stable id or
    /// the folder path; nothing sorts on a rowid or a UID.
    pub fn store_dump(&self, node: &Node) -> Result<serde_json::Value> {
        let compiled = compile(
            node,
            &CompileOptions {
                account: self.account.clone(),
                identities: Vec::new(),
            },
        );

        let mut st = self.conn.prepare(&format!(
            "SELECT m.id, m.thread_id, m.parent_id, m.thread_depth, m.date, m.subject,
                    m.flags, m.size, m.has_attachment, m.message_id,
                    COALESCE(f.path, ''), (m.blob_id IS NOT NULL)
             FROM messages m
             LEFT JOIN folders f ON f.id = m.primary_folder
             WHERE {}
             ORDER BY m.id",
            compiled.sql
        ))?;
        let messages: Vec<serde_json::Value> = st
            .query_map(params_from_iter(compiled.params), |r| {
                Ok(serde_json::json!({
                    "id": r.get::<_, String>(0)?,
                    "thread_id": r.get::<_, Option<String>>(1)?,
                    "parent_id": r.get::<_, Option<String>>(2)?,
                    "depth": r.get::<_, i64>(3)?,
                    "date": format_rfc3339(r.get::<_, i64>(4)?),
                    "subject": r.get::<_, String>(5)?,
                    "flags": flags::unpack(&r.get::<_, String>(6)?),
                    "size": r.get::<_, i64>(7)?,
                    "has_attachment": r.get::<_, i64>(8)? != 0,
                    "message_id": r.get::<_, Option<String>>(9)?,
                    "folder": r.get::<_, String>(10)?,
                    "body_cached": r.get::<_, i64>(11)? != 0,
                }))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        let mut st = self.conn.prepare(
            "SELECT path, name, special_use, rank, total, unread FROM folders ORDER BY path",
        )?;
        let folders: Vec<serde_json::Value> = st
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "path": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "special_use": r.get::<_, Option<String>>(2)?,
                    "rank": r.get::<_, i64>(3)?,
                    "total": r.get::<_, i64>(4)?,
                    "unread": r.get::<_, i64>(5)?,
                }))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        // Placements are keyed by (folder path, stable id) rather than by UID:
        // two devices agree on those and disagree on UIDs.
        let mut st = self.conn.prepare(
            "SELECT f.path, m.id, p.flags FROM placements p
             JOIN folders f ON f.id = p.folder_id
             JOIN messages m ON m.rowid = p.message_rowid
             ORDER BY f.path, m.id",
        )?;
        let placements: Vec<serde_json::Value> = st
            .query_map([], |r| {
                Ok(serde_json::json!({
                    "folder": r.get::<_, String>(0)?,
                    "id": r.get::<_, String>(1)?,
                    "flags": flags::unpack(&r.get::<_, String>(2)?),
                }))
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        // Threads, as sets of members. A thread that groups differently is the
        // single most useful thing this dump can catch.
        let mut threads: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for m in &messages {
            if let (Some(t), Some(i)) = (m["thread_id"].as_str(), m["id"].as_str()) {
                threads
                    .entry(t.to_string())
                    .or_default()
                    .push(i.to_string());
            }
        }

        Ok(serde_json::json!({
            "account": self.account,
            "schema_version": crate::store::schema::SCHEMA_VERSION,
            "counts": {
                "messages": self.message_count()?,
                "unread": self.unread_count()?,
                "folders": self.folder_count()?,
            },
            "folders": folders,
            "messages": messages,
            "placements": placements,
            "threads": threads,
        }))
    }

    /// The account row for `mailbox accounts --json`, filled in from what the
    /// store knows; the caller supplies the parts that live in config.
    pub fn account_summary(
        &self,
        address: &str,
        login_user: &str,
        imap_host: &str,
        imap_port: u16,
    ) -> Account {
        let state = self.sync_state();
        Account {
            id: self.account.clone(),
            address: address.to_string(),
            login_user: login_user.to_string(),
            imap_host: imap_host.to_string(),
            imap_port,
            has_credential: false,
            last_sync: (state.last_full_sync > 0).then(|| format_rfc3339(state.last_full_sync)),
        }
    }
}

fn summary_from_row(r: &Row<'_>, account: &str) -> MessageSummary {
    let addrs: serde_json::Value =
        serde_json::from_str(&r.get::<_, String>(5).unwrap_or_default()).unwrap_or_default();
    let group = |k: &str| -> Vec<Address> {
        serde_json::from_value(addrs.get(k).cloned().unwrap_or(serde_json::Value::Null))
            .unwrap_or_default()
    };
    MessageSummary {
        id: r.get::<_, String>(1).unwrap_or_default(),
        account: account.to_string(),
        folder: r.get::<_, String>(10).unwrap_or_default(),
        thread_id: r
            .get::<_, Option<String>>(2)
            .ok()
            .flatten()
            // A message threaded but not yet rebuilt is its own thread; that is
            // true rather than a placeholder, and keeps the field non-null.
            .unwrap_or_else(|| r.get::<_, String>(1).unwrap_or_default()),
        date: format_rfc3339(r.get::<_, i64>(3).unwrap_or_default()),
        from: group("from"),
        to: group("to"),
        cc: group("cc"),
        subject: r.get::<_, String>(4).unwrap_or_default(),
        flags: flags::unpack(&r.get::<_, String>(6).unwrap_or_default()),
        tags: Vec::new(),
        has_attachment: r.get::<_, i64>(8).unwrap_or(0) != 0,
        size: r.get::<_, i64>(7).unwrap_or(0) as u64,
        preview: r.get::<_, Option<String>>(9).ok().flatten(),
    }
}

/// Builds the cursor that resumes after `last` under `sort`.
///
/// Public because the multi-account merge needs it: the union of several
/// stores' pages is paginated by one cursor, and minting it is the same
/// operation whether one store or several produced the row.
pub fn encode_cursor(last: &MessageSummary, sort: Sort) -> Result<String> {
    let lead = match sort {
        Sort::UnreadFirst => i64::from(last.flags.iter().any(|f| f == flags::SEEN)),
        _ => 0,
    };
    Ok(Cursor {
        lead,
        date: parse_rfc3339_to_unix(&last.date),
        id: last.id.clone(),
    }
    .encode())
}

/// The opaque page cursor of §6.4.
///
/// It carries the whole sort key, not an offset. An offset re-reads the same
/// range of a set that has changed underneath it, which silently repeats or
/// skips rows — precisely the failure an agent paging through a mailbox cannot
/// detect.
#[derive(Debug, Clone)]
struct Cursor {
    lead: i64,
    date: i64,
    id: String,
}

impl Cursor {
    fn encode(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!("{}:{}:{}", self.lead, self.date, self.id))
    }

    fn decode(s: &str) -> Result<Cursor> {
        use base64::Engine;
        let bad = || Error::usage("that cursor is not one of ours");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| bad())?;
        let text = String::from_utf8(bytes).map_err(|_| bad())?;
        let mut parts = text.splitn(3, ':');
        let lead = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
        let date = parts.next().and_then(|p| p.parse().ok()).ok_or_else(bad)?;
        let id = parts.next().ok_or_else(bad)?.to_string();
        if !crate::hex::is_hex(&id) {
            return Err(bad());
        }
        Ok(Cursor { lead, date, id })
    }
}

fn parse_rfc3339_to_unix(s: &str) -> i64 {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map(|d| d.unix_timestamp())
        .unwrap_or(0)
}

/// Re-exported so the CLI can shorten ids without reaching into `id`.
pub use id::short as short_id;

/// Queries the sync engine needs, kept beside the rest of the read path so the
/// column names live in one crate module.
impl Store {
    /// Every UID we hold for a folder. The sync engine diffs this against the
    /// server's list to find expunges when `QRESYNC` is unavailable.
    pub fn placement_uids(&self, folder_id: i64) -> Result<std::collections::BTreeSet<u32>> {
        let mut st = self
            .conn
            .prepare("SELECT uid FROM placements WHERE folder_id = ?1")?;
        let rows = st.query_map([folder_id], |r| r.get::<_, u32>(0))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// The largest UID stored for a folder — where an incremental fetch starts.
    pub fn max_uid(&self, folder_id: i64) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(uid), 0) FROM placements WHERE folder_id = ?1",
            [folder_id],
            |r| r.get::<_, i64>(0),
        )? as u32)
    }

    /// Messages in a folder whose body we have not fetched, newest first.
    ///
    /// `since` implements the prefetch window of §5.1 — bodies for recent mail,
    /// lazily for the rest — and `limit` keeps a first sync of a decade-old
    /// mailbox from turning into a full download nobody asked for.
    pub fn uids_needing_body(
        &self,
        folder_id: i64,
        since: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(u32, String)>> {
        let mut st = self.conn.prepare(
            // `uid IS NOT NULL`: a body cannot be fetched for a placement the
            // server has not named yet.
            "SELECT p.uid, m.id FROM placements p
             JOIN messages m ON m.rowid = p.message_rowid
             WHERE p.folder_id = ?1 AND p.uid IS NOT NULL AND m.blob_id IS NULL
               AND (?2 IS NULL OR m.date >= ?2)
             ORDER BY m.date DESC
             LIMIT ?3",
        )?;
        let rows = st.query_map(rusqlite::params![folder_id, since, limit as i64], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// The `Message-ID` header of a message, for a server-side lookup.
    ///
    /// `None` both when the message is unknown and when it never had one —
    /// callers treat those the same, because neither can be searched for.
    pub fn header_message_id(&self, message_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT message_id FROM messages WHERE id = ?1",
                [message_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    /// Drops every placement in a folder. The `UIDVALIDITY`-reset path (§5.3):
    /// the UIDs we hold have become meaningless, so they go, and the messages
    /// come back on the rescan under their stable ids (§5.2) — which is why a
    /// reset costs bandwidth but not history.
    pub fn clear_folder_placements(&mut self, folder_id: i64) -> Result<u64> {
        self.transaction("clear_folder_placements", |tx| {
            let rowids: Vec<i64> = {
                let mut st =
                    tx.prepare("SELECT message_rowid FROM placements WHERE folder_id = ?1")?;
                let rows = st.query_map([folder_id], |r| r.get::<_, i64>(0))?;
                rows.filter_map(std::result::Result::ok).collect()
            };
            let n = tx.execute("DELETE FROM placements WHERE folder_id = ?1", [folder_id])?;
            for rowid in rowids {
                let remaining: i64 = tx.query_row(
                    "SELECT count(*) FROM placements WHERE message_rowid = ?1",
                    [rowid],
                    |r| r.get(0),
                )?;
                if remaining == 0 {
                    tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [rowid])?;
                    tx.execute("DELETE FROM messages WHERE rowid = ?1", [rowid])?;
                }
            }
            Ok(n as u64)
        })
    }

    /// Folder paths in sync order: the inbox first, then the rest by rank.
    ///
    /// Order is not cosmetic. A sync pass that is interrupted — by `--until`, by
    /// a dropped connection, by the user — should have done the inbox.
    pub fn folder_sync_order(&self) -> Result<Vec<(i64, String, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT id, path, rank FROM folders WHERE selectable = 1 ORDER BY rank, path",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }
}

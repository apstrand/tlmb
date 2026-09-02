//! Writing messages into the store, and keeping the derived state — flags,
//! primary placement, threads, FTS — coherent with them.
//!
//! Every write is an upsert keyed on the stable id of §5.2, which is what makes
//! replaying a sync delta idempotent. That property is load-bearing: it is why
//! `state.toml` may lag `meta.db` (see [`crate::store::state`]) and why a
//! `SIGKILL` mid-pass costs a re-fetch rather than a repair.

use std::collections::{BTreeSet, HashSet};

use rusqlite::{params, Transaction};

use crate::error::Result;
use crate::flags;
use crate::id;
use crate::mime;
use crate::store::Store;
use crate::threading::{self, ThreadInput, ThreadingOptions};
use crate::trace::event;

/// One message as the sync engine fetched it.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub folder_id: i64,
    pub uid: u32,
    /// IMAP flags, unnormalized — [`crate::flags`] handles the dialects.
    pub flags: Vec<String>,
    /// `INTERNALDATE`, unix seconds. The fallback when `Date` is missing or
    /// unparseable, and part of the synthetic id of §5.2.
    pub internaldate: i64,
    /// `RFC822.SIZE`.
    pub size: u64,
    /// `MODSEQ`, when the server offers `CONDSTORE`.
    pub modseq: Option<u64>,
    /// The header block, always. §5.1 fetches headers and structure for
    /// everything and bodies lazily, so this is the common case.
    pub headers: Vec<u8>,
    /// The complete RFC5322 bytes, when the body was fetched too.
    pub raw: Option<Vec<u8>>,
}

/// What [`Store::apply_message`] did.
#[derive(Debug, Clone)]
pub struct Applied {
    pub rowid: i64,
    pub id: String,
    /// False when this was a new placement of a message we already had — the
    /// same mail seen through a second Gmail label, say.
    pub is_new_message: bool,
    /// True when the placement itself was new.
    pub is_new_placement: bool,
}

impl Store {
    /// Upserts one message and its placement.
    ///
    /// Safe to call twice with the same input: the id is derived from the
    /// message, so the second call updates the row it wrote the first time.
    pub fn apply_message(&mut self, incoming: &Incoming) -> Result<Applied> {
        let mut applied = None;
        let account = self.account.clone();
        self.transaction("apply_message", |tx| {
            applied = Some(apply_in_tx(tx, &account, incoming)?);
            Ok(())
        })?;
        Ok(applied.expect("transaction committed without producing a result"))
    }

    /// Upserts a batch inside one transaction, then rethreads what it touched.
    ///
    /// This is the shape the sync engine uses: one transaction per folder pass,
    /// so a crash leaves the store at a folder boundary rather than mid-folder.
    pub fn apply_batch(&mut self, batch: &[Incoming]) -> Result<Vec<Applied>> {
        let account = self.account.clone();
        let out = self.transaction("apply_batch", |tx| {
            let mut out = Vec::with_capacity(batch.len());
            for incoming in batch {
                out.push(apply_in_tx(tx, &account, incoming)?);
            }
            Ok(out)
        })?;

        let seeds: Vec<i64> = out.iter().map(|a| a.rowid).collect();
        self.rebuild_threads(Some(&seeds))?;
        Ok(out)
    }

    /// Attaches fetched body bytes to a message already in the store.
    ///
    /// Re-parses from the full bytes, because the header-only parse could not
    /// see the MIME structure, the attachments, or the text that FTS indexes.
    pub fn set_body(&mut self, message_id: &str, raw: &[u8]) -> Result<()> {
        let blob_id = self.blobs.put(raw)?;
        let parsed = mime::parse(raw);
        self.transaction("set_body", |tx| {
            let rowid: i64 = tx.query_row(
                "SELECT rowid FROM messages WHERE id = ?1",
                [message_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "UPDATE messages SET blob_id = ?2, preview = ?3, has_attachment = ?4 WHERE rowid = ?1",
                params![
                    rowid,
                    blob_id,
                    parsed.preview(200),
                    parsed.has_attachment as i64
                ],
            )?;
            write_attachments(tx, rowid, &parsed)?;
            index_fts(tx, rowid, &parsed)?;
            Ok(())
        })
    }

    /// Removes one placement, and the message with it when that was its last.
    pub fn remove_placement(&mut self, folder_id: i64, uid: u32) -> Result<bool> {
        self.transaction("remove_placement", |tx| {
            let rowid: Option<i64> = tx
                .query_row(
                    "SELECT message_rowid FROM placements WHERE folder_id = ?1 AND uid = ?2",
                    params![folder_id, uid],
                    |r| r.get(0),
                )
                .ok();
            let Some(rowid) = rowid else { return Ok(false) };
            tx.execute(
                "DELETE FROM placements WHERE folder_id = ?1 AND uid = ?2",
                params![folder_id, uid],
            )?;
            let remaining: i64 = tx.query_row(
                "SELECT count(*) FROM placements WHERE message_rowid = ?1",
                [rowid],
                |r| r.get(0),
            )?;
            if remaining == 0 {
                tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [rowid])?;
                tx.execute("DELETE FROM messages WHERE rowid = ?1", [rowid])?;
            } else {
                refresh_primary(tx, rowid)?;
            }
            Ok(true)
        })
    }

    /// Drops every placement in a folder whose UID is not in `keep`.
    ///
    /// This is the expunge path: servers without `QRESYNC` tell us what is
    /// there, not what left, so the difference has to be computed.
    pub fn retain_uids(&mut self, folder_id: i64, keep: &BTreeSet<u32>) -> Result<u64> {
        // Pending placements are excluded: they have no UID *because* the
        // server has not seen them yet, so their absence from the server's
        // listing is expected rather than an expunge. Treating them as expunged
        // would delete a message the moment it was moved into a folder.
        let present: Vec<(u32, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT uid, message_rowid FROM placements
                 WHERE folder_id = ?1 AND uid IS NOT NULL",
            )?;
            let rows = st.query_map([folder_id], |r| {
                Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?))
            })?;
            rows.filter_map(std::result::Result::ok).collect()
        };
        let gone: Vec<(u32, i64)> = present
            .into_iter()
            .filter(|(uid, _)| !keep.contains(uid))
            .collect();
        if gone.is_empty() {
            return Ok(0);
        }
        self.transaction("retain_uids", |tx| {
            for (uid, rowid) in &gone {
                tx.execute(
                    "DELETE FROM placements WHERE folder_id = ?1 AND uid = ?2",
                    params![folder_id, uid],
                )?;
                let remaining: i64 = tx.query_row(
                    "SELECT count(*) FROM placements WHERE message_rowid = ?1",
                    [rowid],
                    |r| r.get(0),
                )?;
                if remaining == 0 {
                    tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [rowid])?;
                    tx.execute("DELETE FROM messages WHERE rowid = ?1", [rowid])?;
                } else {
                    refresh_primary(tx, *rowid)?;
                }
            }
            Ok(())
        })?;
        Ok(gone.len() as u64)
    }

    /// Updates a placement's flags in place — the `FETCH FLAGS` path.
    pub fn set_placement_flags(
        &mut self,
        folder_id: i64,
        uid: u32,
        imap_flags: &[String],
        modseq: Option<u64>,
    ) -> Result<bool> {
        let packed = flags::pack_imap(imap_flags);
        self.transaction("set_placement_flags", |tx| {
            let changed = tx.execute(
                "UPDATE placements SET flags = ?3, modseq = COALESCE(?4, modseq)
                 WHERE folder_id = ?1 AND uid = ?2 AND flags IS NOT ?3",
                params![folder_id, uid, packed, modseq.map(|m| m as i64)],
            )?;
            if changed > 0 {
                let rowid: i64 = tx.query_row(
                    "SELECT message_rowid FROM placements WHERE folder_id = ?1 AND uid = ?2",
                    params![folder_id, uid],
                    |r| r.get(0),
                )?;
                refresh_primary(tx, rowid)?;
            }
            Ok(changed > 0)
        })
    }

    /// Recomputes threads (§5.6).
    ///
    /// With `seeds`, only the conversations those messages belong to are
    /// rebuilt — a sync pass that added ten messages must not pay for a
    /// million. With `None`, everything, which is what `debug` and the
    /// convergence oracle use.
    pub fn rebuild_threads(&mut self, seeds: Option<&[i64]>) -> Result<usize> {
        let opts = ThreadingOptions::default();
        let scope = match seeds {
            Some(s) if !s.is_empty() => Some(expand_scope(&self.conn, s, opts.subject_window)?),
            Some(_) => return Ok(0),
            None => None,
        };

        let rows = load_thread_rows(&self.conn, scope.as_deref())?;
        if rows.is_empty() {
            return Ok(0);
        }
        self.tracer.emit(
            event::THREAD_REBUILD,
            serde_json::json!({ "messages": rows.len(), "scoped": scope.is_some() }),
        );

        let inputs: Vec<ThreadInput<'_>> = rows
            .iter()
            .map(|r| ThreadInput {
                id: &r.id,
                message_id: r.message_id.as_deref(),
                in_reply_to: r.in_reply_to.as_deref(),
                references: &r.refs,
                subject: &r.subject,
                date: r.date,
            })
            .collect();

        let result = threading::thread(&inputs, &opts);
        let n = result.assignments.len();
        self.transaction("rebuild_threads", |tx| {
            let mut st = tx.prepare(
                "UPDATE messages SET thread_id = ?2, parent_id = ?3, thread_depth = ?4 WHERE id = ?1",
            )?;
            for a in &result.assignments {
                st.execute(params![a.id, a.thread_id, a.parent_id, a.depth])?;
            }
            Ok(())
        })?;
        Ok(n)
    }
}

fn apply_in_tx(tx: &Transaction<'_>, account: &str, incoming: &Incoming) -> Result<Applied> {
    let bytes = incoming.raw.as_deref().unwrap_or(&incoming.headers);
    let parsed = mime::parse(bytes);

    let date = parsed.date.unwrap_or(incoming.internaldate);
    let from_addr = parsed
        .from
        .first()
        .map(|a| a.address.to_ascii_lowercase())
        .unwrap_or_default();
    let msg_id = id::message_id(
        account,
        parsed.message_id.as_deref(),
        &id::Fallback {
            date: incoming.internaldate,
            from: &from_addr,
            subject: &parsed.subject,
            size: incoming.size,
        },
    );

    let existing: Option<i64> = tx
        .query_row("SELECT rowid FROM messages WHERE id = ?1", [&msg_id], |r| {
            r.get(0)
        })
        .ok();
    let is_new_message = existing.is_none();

    let addrs = serde_json::json!({
        "from": parsed.from,
        "to": parsed.to,
        "cc": parsed.cc,
        "bcc": parsed.bcc,
        "reply_to": parsed.reply_to,
    })
    .to_string();
    let subject_norm = threading::normalize_subject(&parsed.subject);
    let refs_json = serde_json::to_string(&parsed.references)?;

    // The header-only parse cannot see attachments, so a message whose body has
    // not been fetched reports `has_attachment` from the structure the server
    // gave us — which for M1 means "not yet known" until the body lands.
    let has_attachment = parsed.has_attachment as i64;
    let preview = parsed.preview(200);

    let rowid = match existing {
        Some(rowid) => {
            tx.execute(
                "UPDATE messages SET
                    message_id = ?2, in_reply_to = ?3, refs = ?4, date = ?5,
                    subject = ?6, subject_norm = ?7, addrs = ?8, size = ?9,
                    has_attachment = MAX(has_attachment, ?10),
                    preview = COALESCE(?11, preview)
                 WHERE rowid = ?1",
                params![
                    rowid,
                    parsed.message_id,
                    parsed.in_reply_to,
                    refs_json,
                    date,
                    parsed.subject,
                    subject_norm,
                    addrs,
                    incoming.size as i64,
                    has_attachment,
                    preview,
                ],
            )?;
            rowid
        }
        None => {
            tx.execute(
                "INSERT INTO messages
                    (id, message_id, in_reply_to, refs, date, subject, subject_norm,
                     addrs, flags, size, has_attachment, preview)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ' ', ?9, ?10, ?11)",
                params![
                    msg_id,
                    parsed.message_id,
                    parsed.in_reply_to,
                    refs_json,
                    date,
                    parsed.subject,
                    subject_norm,
                    addrs,
                    incoming.size as i64,
                    has_attachment,
                    preview,
                ],
            )?;
            tx.last_insert_rowid()
        }
    };

    write_addresses(tx, rowid, &parsed)?;
    write_refs(tx, rowid, &parsed)?;
    if incoming.raw.is_some() {
        write_attachments(tx, rowid, &parsed)?;
    }
    index_fts(tx, rowid, &parsed)?;

    // The server is authoritative about what a UID means. If it says this UID
    // now names a different message, whatever we had there is stale — a UID
    // reused without a UIDVALIDITY bump is a server misbehaving, but the
    // recovery is obvious and the alternative is a raw constraint failure that
    // aborts the whole pass.
    let displaced: Option<i64> = tx
        .query_row(
            "SELECT message_rowid FROM placements
             WHERE folder_id = ?1 AND uid = ?2 AND message_rowid <> ?3",
            params![incoming.folder_id, incoming.uid, rowid],
            |r| r.get(0),
        )
        .ok();
    if let Some(displaced) = displaced {
        tx.execute(
            "DELETE FROM placements WHERE folder_id = ?1 AND message_rowid = ?2",
            params![incoming.folder_id, displaced],
        )?;
        // Losing its last placement means the message is nowhere, and a message
        // that is nowhere is gone — the same rule `remove_placement` follows.
        // Without this the row survives with no folder, showing up in listings
        // as mail that exists on no server.
        let remaining: i64 = tx.query_row(
            "SELECT count(*) FROM placements WHERE message_rowid = ?1",
            [displaced],
            |r| r.get(0),
        )?;
        if remaining == 0 {
            tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [displaced])?;
            tx.execute("DELETE FROM messages WHERE rowid = ?1", [displaced])?;
        } else {
            refresh_primary(tx, displaced)?;
        }
    }

    // Keyed on (folder, message), not (folder, uid): a message appears at most
    // once in a folder, and a placement created by a local move (§5.3) is
    // already there without a UID. Upserting on the UID would insert a second
    // row for the same message the moment the server finally named it.
    let existing_placement: i64 = tx.query_row(
        "SELECT count(*) FROM placements WHERE folder_id = ?1 AND message_rowid = ?2",
        params![incoming.folder_id, rowid],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT INTO placements (folder_id, uid, message_rowid, flags, modseq, internaldate)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(folder_id, message_rowid) DO UPDATE SET
            uid           = excluded.uid,
            flags         = excluded.flags,
            modseq        = excluded.modseq,
            internaldate  = excluded.internaldate",
        params![
            incoming.folder_id,
            incoming.uid,
            rowid,
            flags::pack_imap(&incoming.flags),
            incoming.modseq.map(|m| m as i64),
            incoming.internaldate,
        ],
    )?;

    refresh_primary(tx, rowid)?;
    record_contacts(tx, &parsed, date)?;

    Ok(Applied {
        rowid,
        id: msg_id,
        is_new_message,
        is_new_placement: existing_placement == 0,
    })
}

/// Chooses the placement a message is *reported* as living in, and copies its
/// flags up to the message row.
///
/// The rank ordering is what deduplicates `[Gmail]/All Mail` against the label
/// folders (§5.3): every Gmail message is in All Mail, and reporting that as
/// its folder would make the unified inbox useless.
pub(crate) fn refresh_primary(tx: &Transaction<'_>, rowid: i64) -> Result<()> {
    let primary: Option<(i64, String)> = tx
        .query_row(
            "SELECT p.folder_id, p.flags FROM placements p
             JOIN folders f ON f.id = p.folder_id
             WHERE p.message_rowid = ?1
             ORDER BY f.rank, f.id
             LIMIT 1",
            [rowid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match primary {
        Some((folder_id, flags)) => {
            tx.execute(
                "UPDATE messages SET primary_folder = ?2, flags = ?3 WHERE rowid = ?1",
                params![rowid, folder_id, flags],
            )?;
        }
        None => {
            tx.execute(
                "UPDATE messages SET primary_folder = NULL WHERE rowid = ?1",
                [rowid],
            )?;
        }
    }
    Ok(())
}

fn write_addresses(tx: &Transaction<'_>, rowid: i64, parsed: &mime::Parsed) -> Result<()> {
    tx.execute("DELETE FROM addresses WHERE message_rowid = ?1", [rowid])?;
    let mut st = tx.prepare(
        "INSERT INTO addresses (message_rowid, field, name, addr, ord) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (field, list) in [
        ("from", &parsed.from),
        ("to", &parsed.to),
        ("cc", &parsed.cc),
        ("bcc", &parsed.bcc),
        ("reply_to", &parsed.reply_to),
    ] {
        for (i, a) in list.iter().enumerate() {
            st.execute(params![
                rowid,
                field,
                a.name.clone().unwrap_or_default(),
                a.address.to_ascii_lowercase(),
                i as i64
            ])?;
        }
    }
    Ok(())
}

fn write_refs(tx: &Transaction<'_>, rowid: i64, parsed: &mime::Parsed) -> Result<()> {
    tx.execute("DELETE FROM message_refs WHERE message_rowid = ?1", [rowid])?;
    let mut st =
        tx.prepare("INSERT INTO message_refs (message_rowid, ref, ord) VALUES (?1, ?2, ?3)")?;
    let mut ord = 0i64;
    for r in &parsed.references {
        st.execute(params![rowid, r, ord])?;
        ord += 1;
    }
    if let Some(irt) = &parsed.in_reply_to {
        if !parsed.references.contains(irt) {
            st.execute(params![rowid, irt, ord])?;
        }
    }
    Ok(())
}

fn write_attachments(tx: &Transaction<'_>, rowid: i64, parsed: &mime::Parsed) -> Result<()> {
    tx.execute("DELETE FROM attachments WHERE message_rowid = ?1", [rowid])?;
    let mut st = tx.prepare(
        "INSERT INTO attachments
            (message_rowid, part, filename, filename_lc, mime_type, size, content_id, is_attachment)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for a in &parsed.attachments {
        st.execute(params![
            rowid,
            a.part,
            a.filename,
            a.filename.as_ref().map(|f| f.to_ascii_lowercase()),
            a.mime_type,
            a.size as i64,
            a.content_id,
            a.is_attachment as i64,
        ])?;
    }
    Ok(())
}

/// Replaces this message's FTS row. Contentless FTS5 has no `UPDATE`, so the
/// delete-then-insert is the supported idiom rather than a workaround.
fn index_fts(tx: &Transaction<'_>, rowid: i64, parsed: &mime::Parsed) -> Result<()> {
    tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", [rowid])?;
    tx.execute(
        "INSERT INTO messages_fts (rowid, body) VALUES (?1, ?2)",
        params![rowid, parsed.fts_text()],
    )?;
    Ok(())
}

/// Feeds the address book (§5.1): derived from mail traffic, never from a
/// contacts server (§2).
fn record_contacts(tx: &Transaction<'_>, parsed: &mime::Parsed, date: i64) -> Result<()> {
    let mut st = tx.prepare(
        "INSERT INTO contacts (addr, name, seen_count, last_seen)
         VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(addr) DO UPDATE SET
            seen_count = seen_count + 1,
            last_seen  = MAX(last_seen, excluded.last_seen),
            -- Keep the first non-empty display name we saw rather than the
            -- latest: senders who sign mail from a phone often send an empty
            -- one, and losing the name to that is worse than being stale.
            name = CASE WHEN contacts.name = '' THEN excluded.name ELSE contacts.name END",
    )?;
    for a in parsed
        .from
        .iter()
        .chain(&parsed.to)
        .chain(&parsed.cc)
        .chain(&parsed.reply_to)
    {
        st.execute(params![
            a.address.to_ascii_lowercase(),
            a.name.clone().unwrap_or_default(),
            date
        ])?;
    }
    Ok(())
}

struct ThreadRow {
    id: String,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    refs: Vec<String>,
    subject: String,
    date: i64,
}

fn load_thread_rows(conn: &rusqlite::Connection, scope: Option<&[i64]>) -> Result<Vec<ThreadRow>> {
    let (sql, params): (String, Vec<rusqlite::types::Value>) = match scope {
        Some(ids) => (
            format!(
                "SELECT id, message_id, in_reply_to, refs, subject, date FROM messages
                 WHERE rowid IN ({})",
                placeholders(ids.len())
            ),
            ids.iter()
                .map(|i| rusqlite::types::Value::Integer(*i))
                .collect(),
        ),
        None => (
            "SELECT id, message_id, in_reply_to, refs, subject, date FROM messages".to_string(),
            Vec::new(),
        ),
    };
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(params), |r| {
        Ok(ThreadRow {
            id: r.get(0)?,
            message_id: r.get(1)?,
            in_reply_to: r.get(2)?,
            refs: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            subject: r.get(4)?,
            date: r.get(5)?,
        })
    })?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

/// Grows a seed set into every message that could end up in the same thread.
///
/// Threading is not a local decision: adding one reply can merge two existing
/// threads, so rebuilding only the seeds would produce a tree that disagrees
/// with the one a full rebuild produces. The closure below is what keeps a
/// scoped rebuild and a full rebuild identical — which the convergence oracle
/// (§14.3) checks by comparing the two.
fn expand_scope(
    conn: &rusqlite::Connection,
    seeds: &[i64],
    subject_window: i64,
) -> Result<Vec<i64>> {
    let mut scope: HashSet<i64> = seeds.iter().copied().collect();
    let mut frontier: Vec<i64> = seeds.to_vec();

    while !frontier.is_empty() {
        let mut next: HashSet<i64> = HashSet::new();
        let ph = placeholders(frontier.len());
        let vals: Vec<rusqlite::types::Value> = frontier
            .iter()
            .map(|i| rusqlite::types::Value::Integer(*i))
            .collect();

        // Everything already assigned to the same thread as a frontier message.
        collect(
            conn,
            &format!(
                "SELECT rowid FROM messages WHERE thread_id IN
                    (SELECT thread_id FROM messages WHERE rowid IN ({ph}) AND thread_id IS NOT NULL)"
            ),
            &vals,
            &mut next,
        )?;

        // Anything the frontier's Message-IDs are referenced by, or that the
        // frontier's References point at.
        collect(
            conn,
            &format!(
                "SELECT r.message_rowid FROM message_refs r WHERE r.ref IN
                    (SELECT message_id FROM messages WHERE rowid IN ({ph}) AND message_id IS NOT NULL)"
            ),
            &vals,
            &mut next,
        )?;
        collect(
            conn,
            &format!(
                "SELECT m.rowid FROM messages m WHERE m.message_id IN
                    (SELECT ref FROM message_refs WHERE message_rowid IN ({ph}))"
            ),
            &vals,
            &mut next,
        )?;

        // Subject grouping can join otherwise unlinked messages (§5.6), so the
        // window has to come along or a scoped rebuild would miss the join.
        let mut window_vals = vals.clone();
        window_vals.push(rusqlite::types::Value::Integer(subject_window));
        collect(
            conn,
            &format!(
                "SELECT m.rowid FROM messages m
                 JOIN messages s ON s.rowid IN ({ph})
                 WHERE m.subject_norm <> ''
                   AND m.subject_norm = s.subject_norm
                   AND ABS(m.date - s.date) <= ?{}",
                frontier.len() + 1
            ),
            &window_vals,
            &mut next,
        )?;

        frontier = next.into_iter().filter(|id| scope.insert(*id)).collect();
    }
    let mut out: Vec<i64> = scope.into_iter().collect();
    // Deterministic order, so a scoped rebuild hashes the same as a full one.
    out.sort_unstable();
    Ok(out)
}

fn collect(
    conn: &rusqlite::Connection,
    sql: &str,
    vals: &[rusqlite::types::Value],
    out: &mut HashSet<i64>,
) -> Result<()> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(vals.iter().cloned()), |r| {
        r.get::<_, i64>(0)
    })?;
    for row in rows.flatten() {
        out.insert(row);
    }
    Ok(())
}

pub(crate) fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

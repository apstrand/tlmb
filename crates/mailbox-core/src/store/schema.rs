//! `meta.db`'s schema and its migrations.
//!
//! One SQLite file holds messages, folders, flags, threads, the address book,
//! and the FTS5 index (§5.1). The index lives here rather than in a second
//! engine so there is one file, one fsync domain, and nothing to keep coherent
//! with the store — and so it runs unchanged on mobile (§16).

use rusqlite::Connection;

use crate::error::Result;
use crate::trace::{event, Tracer};

/// Bumped whenever [`MIGRATIONS`] grows. A store from the future is refused
/// rather than opened read-only: a newer version may have written columns this
/// build would silently drop on the next write.
pub const SCHEMA_VERSION: u32 = 7;

/// Each entry is applied in order inside one transaction. Migrations are
/// append-only — editing an existing one changes what an already-migrated store
/// looks like, which is a divergence no test will catch.
const MIGRATIONS: &[&str] = &[
    // v1 — M1: the read-side store.
    r#"
    CREATE TABLE folders (
        id            INTEGER PRIMARY KEY,
        -- As the server spells it, delimiter and all.
        path          TEXT    NOT NULL UNIQUE,
        -- Last segment, decoded from modified UTF-7.
        name          TEXT    NOT NULL,
        delimiter     TEXT,
        -- From SPECIAL-USE where advertised, guessed from the name otherwise.
        special_use   TEXT,
        selectable    INTEGER NOT NULL DEFAULT 1,
        -- Which placement wins when a message is filed in several folders:
        -- 0 inbox, 1 other special-use, 2 ordinary, 3 catch-alls like
        -- [Gmail]/All Mail. This is what deduplicates All Mail against the
        -- label folders (§5.3) without the sync engine having to know about it.
        rank          INTEGER NOT NULL DEFAULT 2,
        total         INTEGER NOT NULL DEFAULT 0,
        unread        INTEGER NOT NULL DEFAULT 0,
        subscribed    INTEGER NOT NULL DEFAULT 1
    );

    CREATE TABLE messages (
        rowid          INTEGER PRIMARY KEY,
        -- The stable id of §5.2.
        id             TEXT    NOT NULL UNIQUE,
        thread_id      TEXT,
        -- The parent within the locally known set, from JWZ (§5.6).
        parent_id      TEXT,
        thread_depth   INTEGER NOT NULL DEFAULT 0,
        message_id     TEXT,
        in_reply_to    TEXT,
        -- JSON array of normalized References, in header order.
        refs           TEXT    NOT NULL DEFAULT '[]',
        date           INTEGER NOT NULL,
        subject        TEXT    NOT NULL DEFAULT '',
        -- Reply/forward prefixes stripped and case folded (§5.6). Stored
        -- rather than computed so subject grouping can be an indexed lookup
        -- instead of a scan of every candidate root.
        subject_norm   TEXT    NOT NULL DEFAULT '',
        -- JSON of every address group, for rendering without a re-parse.
        addrs          TEXT    NOT NULL DEFAULT '{}',
        -- Space-delimited and space-padded (' seen flagged '), so a
        -- containment test cannot confuse `seen` with `unseen`. These are the
        -- primary placement's flags; see `placements` for the per-folder truth.
        flags          TEXT    NOT NULL DEFAULT ' ',
        size           INTEGER NOT NULL DEFAULT 0,
        has_attachment INTEGER NOT NULL DEFAULT 0,
        -- The content address of the raw MIME in the CAS. NULL until the body
        -- is fetched; §5.1 fetches bodies lazily.
        blob_id        TEXT,
        preview        TEXT,
        primary_folder INTEGER REFERENCES folders(id) ON DELETE SET NULL
    );
    -- The listing order of §6.4, so `ls` is an index scan.
    CREATE INDEX messages_date    ON messages(date DESC, id DESC);
    CREATE INDEX messages_thread  ON messages(thread_id, date);
    CREATE INDEX messages_msgid   ON messages(message_id);
    CREATE INDEX messages_blob    ON messages(blob_id);
    CREATE INDEX messages_subject ON messages(subject_norm, date);

    -- The References closure, exploded. Threading needs "every message that
    -- names this Message-ID" to be an index lookup: without it, a rebuild
    -- scoped to the messages a sync pass touched would have to scan the whole
    -- mailbox to find their relatives.
    CREATE TABLE message_refs (
        message_rowid INTEGER NOT NULL REFERENCES messages(rowid) ON DELETE CASCADE,
        ref           TEXT    NOT NULL,
        ord           INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX message_refs_ref ON message_refs(ref);
    CREATE INDEX message_refs_msg ON message_refs(message_rowid);

    -- One row per (folder, uid). A message filed in several folders — or
    -- carrying several Gmail labels — is one `messages` row and several of
    -- these, which is what makes the blob store it once (§5.1).
    CREATE TABLE placements (
        folder_id     INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        uid           INTEGER NOT NULL,
        message_rowid INTEGER NOT NULL REFERENCES messages(rowid) ON DELETE CASCADE,
        flags         TEXT    NOT NULL DEFAULT ' ',
        modseq        INTEGER,
        internaldate  INTEGER,
        PRIMARY KEY (folder_id, uid)
    ) WITHOUT ROWID;
    CREATE INDEX placements_msg ON placements(message_rowid);

    CREATE TABLE addresses (
        message_rowid INTEGER NOT NULL REFERENCES messages(rowid) ON DELETE CASCADE,
        -- from | to | cc | bcc | reply_to
        field         TEXT    NOT NULL,
        name          TEXT    NOT NULL DEFAULT '',
        addr          TEXT    NOT NULL,
        -- Position in the header, so rendering can restore the original order.
        ord           INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX addresses_msg  ON addresses(message_rowid);
    CREATE INDEX addresses_addr ON addresses(addr);

    CREATE TABLE attachments (
        message_rowid INTEGER NOT NULL REFERENCES messages(rowid) ON DELETE CASCADE,
        -- IMAP part number (`2.1`), which is what `attach <id> <part>` takes.
        part          TEXT    NOT NULL,
        filename      TEXT,
        -- Lowercased filename, because `filename:*.PDF` and `filename:*.pdf`
        -- are the same question and GLOB is case-sensitive.
        filename_lc   TEXT,
        mime_type     TEXT    NOT NULL DEFAULT 'application/octet-stream',
        size          INTEGER NOT NULL DEFAULT 0,
        content_id    TEXT,
        is_attachment INTEGER NOT NULL DEFAULT 1
    );
    CREATE INDEX attachments_msg ON attachments(message_rowid);

    -- Local tags (§5.4). Local-only until M8 gives them the state folder.
    CREATE TABLE tags (
        message_rowid INTEGER NOT NULL REFERENCES messages(rowid) ON DELETE CASCADE,
        tag           TEXT    NOT NULL,
        PRIMARY KEY (message_rowid, tag)
    ) WITHOUT ROWID;

    -- The address book of §5.1: derived from mail traffic, never from a
    -- contacts server (§2).
    CREATE TABLE contacts (
        addr        TEXT    PRIMARY KEY,
        name        TEXT    NOT NULL DEFAULT '',
        -- How often we have seen it, and when last, so completion can rank.
        seen_count  INTEGER NOT NULL DEFAULT 0,
        last_seen   INTEGER NOT NULL DEFAULT 0,
        -- Counted separately: someone you write to outranks someone who writes
        -- to you.
        sent_count  INTEGER NOT NULL DEFAULT 0
    ) WITHOUT ROWID;

    -- Contentless FTS5 keyed on messages.rowid. Contentless keeps the index
    -- from duplicating every body, and `contentless_delete` is what lets a
    -- message be removed without a full rebuild.
    CREATE VIRTUAL TABLE messages_fts USING fts5(
        body,
        content='',
        contentless_delete=1,
        tokenize='unicode61 remove_diacritics 2'
    );
    "#,
    // v2 — M2: the write path.
    //
    // Two changes, both forced by §5.3's "writes are optimistic". A message
    // moved locally is in its destination *now*, before any server has agreed —
    // so a placement has to be able to exist without a UID, which the v1
    // primary key forbade. And the operation queue needs somewhere to live.
    r#"
    -- Rebuilt rather than altered: SQLite cannot drop a primary key, and the
    -- natural key was wrong anyway. A message appears at most once in a folder,
    -- so (folder_id, message_rowid) is the real identity; the UID is what the
    -- server calls it, which is a different question and sometimes has no
    -- answer yet.
    CREATE TABLE placements_v2 (
        id            INTEGER PRIMARY KEY,
        folder_id     INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        -- NULL while a local move has not yet been confirmed by the server.
        uid           INTEGER,
        message_rowid INTEGER NOT NULL REFERENCES messages(rowid) ON DELETE CASCADE,
        flags         TEXT    NOT NULL DEFAULT ' ',
        modseq        INTEGER,
        internaldate  INTEGER
    );
    INSERT INTO placements_v2 (folder_id, uid, message_rowid, flags, modseq, internaldate)
        SELECT folder_id, uid, message_rowid, flags, modseq, internaldate FROM placements;

    -- v1 keyed placements on (folder_id, uid), which let one message sit in one
    -- folder twice under different UIDs — a re-`APPEND`, or a rescan after a
    -- `UIDVALIDITY` change, produced exactly that. The new key is
    -- (folder_id, message_rowid), so those rows have to be collapsed before the
    -- unique index exists or the migration fails on perfectly ordinary data.
    --
    -- The highest UID wins: it is the most recent thing the server said about
    -- the message, and the lower one is the stale copy.
    DELETE FROM placements_v2 WHERE id NOT IN (
        SELECT id FROM (
            SELECT id, ROW_NUMBER() OVER (
                PARTITION BY folder_id, message_rowid ORDER BY uid DESC
            ) AS rn
            FROM placements_v2
        ) WHERE rn = 1
    );

    DROP TABLE placements;
    ALTER TABLE placements_v2 RENAME TO placements;

    CREATE UNIQUE INDEX placements_folder_msg ON placements(folder_id, message_rowid);
    -- Partial, so any number of placements may be awaiting a UID at once. A
    -- plain unique index would let one folder hold exactly one pending move.
    CREATE UNIQUE INDEX placements_folder_uid  ON placements(folder_id, uid)
        WHERE uid IS NOT NULL;
    CREATE INDEX placements_msg ON placements(message_rowid);

    -- The operation queue of §5.3. Every write lands here and is replayed with
    -- idempotent semantics on reconnect.
    --
    -- Operations name a message by its stable id (§5.2) and a folder by its
    -- path — never by UID. A UID is resolved at replay time, so a queue that
    -- outlives a UIDVALIDITY reset still means what it meant when it was
    -- written, and a replayed operation is a no-op rather than a second edit.
    CREATE TABLE ops (
        id           INTEGER PRIMARY KEY,
        -- flag | move | copy | delete
        kind         TEXT    NOT NULL,
        message_id   TEXT    NOT NULL,
        -- The folder the message was in when the operation was queued.
        folder_path  TEXT,
        -- The UID it had there, and the UIDVALIDITY that UID belongs to.
        --
        -- An optimistic move deletes the source placement immediately, so by
        -- replay time the store no longer knows what the server calls the
        -- message in the folder it is being moved out of. Recording it here is
        -- the cheap path; the UIDVALIDITY is what says whether it is still
        -- true, and a `SEARCH` by `Message-ID` is the fallback when it is not.
        source_uid          INTEGER,
        source_uidvalidity  INTEGER,
        -- Destination, for move and copy.
        target_path  TEXT,
        -- Kind-specific JSON: the flags to add and remove, for `flag`.
        payload      TEXT    NOT NULL DEFAULT '{}',
        -- pending | done | failed
        state        TEXT    NOT NULL DEFAULT 'pending',
        attempts     INTEGER NOT NULL DEFAULT 0,
        last_error   TEXT,
        created      INTEGER NOT NULL,
        -- Unix seconds before which a retry must not be attempted (backoff).
        next_attempt INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX ops_pending ON ops(state, next_attempt);
    CREATE INDEX ops_message ON ops(message_id);
    "#,
    // v3 — M2: the outbox (§5.5).
    r#"
    -- Outgoing mail goes here first and only then to SMTP submission. The point
    -- is durability: an entry survives a restart, a crash, and a week offline,
    -- because the alternative to a durable outbox is a composed message that
    -- exists only in a process that has since died.
    CREATE TABLE outbox (
        id              INTEGER PRIMARY KEY,
        -- The stable id (§5.2) of the built message.
        message_id      TEXT    NOT NULL,
        -- The RFC5322 bytes live in the same content-addressed store as
        -- received mail (§5.1), so a sent message costs one copy whether it is
        -- in the outbox, in Sent, or both.
        blob_id         TEXT    NOT NULL,
        -- The SMTP envelope, which is *not* the To/Cc headers: Bcc recipients
        -- appear here and nowhere else, which is the whole of what makes Bcc
        -- blind.
        envelope_from   TEXT    NOT NULL,
        envelope_to     TEXT    NOT NULL DEFAULT '[]',
        -- draft | pending_approval | queued | sent | failed | cancelled
        state           TEXT    NOT NULL DEFAULT 'queued',
        -- §6.5. Unique while set, so a retry with the same key cannot enqueue a
        -- second entry even if two processes race.
        idempotency_key TEXT,
        -- The reply the original call produced, returned verbatim to a repeat
        -- of the same key rather than sending again.
        result          TEXT,
        attempts        INTEGER NOT NULL DEFAULT 0,
        last_error      TEXT,
        created         INTEGER NOT NULL,
        -- The undo-send window (§5.5): not eligible before this instant.
        send_after      INTEGER NOT NULL DEFAULT 0,
        next_attempt    INTEGER NOT NULL DEFAULT 0,
        sent_at         INTEGER,
        -- Whether the Sent copy has been APPENDed (§5.5). Separate from `state`
        -- because a message can be delivered and not yet filed, and the second
        -- half must not be able to un-send the first.
        sent_uploaded   INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX outbox_state ON outbox(state, next_attempt);
    CREATE UNIQUE INDEX outbox_idempotency ON outbox(idempotency_key)
        WHERE idempotency_key IS NOT NULL;
    "#,
    // v4 — M3: make the default listing indexable.
    //
    // `ls` sorts unread-first (§6.1), which was written as
    // `ORDER BY instr(flags, ' seen ') > 0`. An expression cannot be served by
    // an index, so every listing was a full table scan followed by a full sort:
    // fine at a thousand messages, and 82% of §10's 30 ms budget at a hundred
    // thousand — measured, not guessed, by `debug bench`.
    r#"
    -- A *generated* column rather than a maintained one. The value is defined
    -- by `flags` and derived from it, so it cannot drift the way a column
    -- updated by hand in four places eventually does. VIRTUAL costs nothing to
    -- store and can still be indexed, which is the only property that matters
    -- here.
    ALTER TABLE messages
        ADD COLUMN seen INTEGER GENERATED ALWAYS AS (instr(flags, ' seen ') > 0) VIRTUAL;

    -- The exact key `ls` orders by, so SQLite walks the index and stops at the
    -- page size instead of sorting the whole mailbox to return fifty rows.
    CREATE INDEX messages_unread ON messages(seen, date DESC, id DESC);

    -- The same trick for the per-folder unread counts, which `sync` recomputes
    -- every pass and `status` reads. Without it, counting unread mail in eight
    -- folders is eight scans of a million rows.
    ALTER TABLE placements
        ADD COLUMN seen INTEGER GENERATED ALWAYS AS (instr(flags, ' seen ') > 0) VIRTUAL;
    CREATE INDEX placements_unread ON placements(folder_id, seen);
    "#,
    // v5 — M3: desktop notifications (§11).
    r#"
    -- Whether the user has already been told about this message.
    --
    -- A column rather than a high-water mark, because mail does not arrive in
    -- date order: a message delivered late but dated last week would slip under
    -- any timestamp watermark and never be announced.
    ALTER TABLE messages ADD COLUMN notified INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX messages_unnotified ON messages(notified, date DESC) WHERE notified = 0;

    "#,
    // v6 — M5: snooze, in the local-only form §7 describes.
    r#"
    -- Local state, and it stays local until the §5.4 state folder lands in M8.
    -- Nothing here is sent to the server: a snooze is a timer plus a row, so a
    -- snoozed message reappears on the device that snoozed it and nowhere else.
    -- That is a real limitation rather than an implementation gap, which is why
    -- `device` is stored: every surface that shows a snooze names the machine
    -- holding it, instead of implying it is synced.
    CREATE TABLE snoozed (
        message_rowid INTEGER PRIMARY KEY REFERENCES messages(rowid) ON DELETE CASCADE,
        -- Unix seconds. The message returns to the inbox at or after this.
        until         INTEGER NOT NULL,
        -- Which machine will wake it. Shown, never used for a decision.
        device        TEXT    NOT NULL,
        created       INTEGER NOT NULL
    ) WITHOUT ROWID;

    -- The default listing asks "which of these are asleep right now" on every
    -- paint, so the due check is an index seek rather than a scan of the
    -- snooze table.
    CREATE INDEX snoozed_due ON snoozed(until);
    "#,
    // v7 — M7: the change feed behind `mailbox watch` (§6.4).
    r#"
    -- What happened to the mailbox, in order. `watch` streams this so an agent
    -- can follow a mailbox without polling `ls` and diffing it.
    --
    -- Durable and cursored rather than an in-process broadcast, because the
    -- consumer §6.4 is written for is a program that will be restarted: one
    -- that resumes from the last `seq` it acted on misses nothing, and one that
    -- crashes mid-event redelivers rather than skips. The same rows are what
    -- lets `--since` answer "what did I miss" for a caller that was not
    -- running at all.
    CREATE TABLE changes (
        -- The `--cursor` of §6.4: monotonic, opaque, never reused.
        seq        INTEGER PRIMARY KEY AUTOINCREMENT,
        at         INTEGER NOT NULL,
        -- new | flag | move | copy | tag | sent | deleted
        kind       TEXT    NOT NULL,
        message_id TEXT,
        -- The folder the change is about: where it arrived, or where it went.
        folder     TEXT,
        -- Whether this device made the change or a sync brought it in. An
        -- agent that acts on its own writes would otherwise loop.
        local      INTEGER NOT NULL DEFAULT 0,
        -- Kind-specific JSON: the flags after a `flag`, the source of a `move`.
        payload    TEXT    NOT NULL DEFAULT '{}'
    );
    CREATE INDEX changes_at ON changes(at);
    "#,
];

/// Creates or upgrades the schema, in one transaction per migration.
pub fn migrate(conn: &mut Connection, tracer: &Tracer) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL is the right durability for a cache of server-side state: a lost
    // final transaction re-syncs, and FULL costs an fsync per commit on a
    // workload that commits per folder.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    let current: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(crate::Error::store(format!(
            concat!(
                "this store was written by a newer mailbox (schema v{}, this build speaks v{}); ",
                "upgrade mailbox, or delete the store and re-sync"
            ),
            current, SCHEMA_VERSION
        )));
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }

    for (i, ddl) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let to = i as u32 + 1;
        tracer.emit(
            event::STORE_MIGRATE,
            serde_json::json!({ "from": current, "to": to }),
        );
        let tx = conn.transaction()?;
        // A migration is all-or-nothing: a half-applied one would leave a store
        // that neither opens nor can be repaired. The cost of that guarantee is
        // that a failure leaves the store at its old version, so the same
        // failure recurs — which is why the error says what to do about it.
        tx.execute_batch(ddl).map_err(|e| {
            crate::Error::store(format!(
                concat!(
                    "upgrading the store from v{} to v{} failed: {}. The store is a cache of ",
                    "what is on the server, so deleting the account's directory and re-syncing ",
                    "is a safe repair."
                ),
                current, to, e
            ))
        })?;
        tx.pragma_update(None, "user_version", to)?;
        tx.commit()?;
    }
    Ok(())
}

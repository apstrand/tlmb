-- The schema exactly as M1 shipped it (commit 3a81efa), frozen.
--
-- Kept as a file rather than reconstructed in a test, because the point of the
-- migration tests is to upgrade what real users actually have. A hand-written
-- approximation would pass while the real thing failed, which is precisely what
-- happened before these tests existed.

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
    
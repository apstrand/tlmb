//! Upgrading a store written by an older build.
//!
//! These exist because the migration list was mis-split for two releases and
//! nobody noticed: a *fresh* store ended up with the right schema, so every
//! test passed, while no store an earlier build had actually written could be
//! upgraded at all. A migration suite that only ever migrates from nothing
//! tests nothing.
//!
//! So the fixture is the real thing — `fixtures/schema_v1.sql` is the schema
//! exactly as M1 shipped it — and the assertions are about data an older build
//! could legitimately have stored.

use mailbox_core::clock::Clock;
use mailbox_core::store::Store;
use mailbox_core::Tracer;

/// The schema as M1 shipped it, frozen.
const SCHEMA_V1: &str = include_str!("fixtures/schema_v1.sql");

mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> TempDir {
            let mut seed = [0u8; 8];
            getrandom::fill(&mut seed).expect("entropy");
            let path =
                std::env::temp_dir().join(format!("mailbox-migrate-{}", u64::from_le_bytes(seed)));
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// Builds a store at schema v1, then runs `extra` against it.
fn v1_store(dir: &std::path::Path, extra: &str) {
    std::fs::create_dir_all(dir.join("blobs")).expect("blobs");
    let conn = rusqlite::Connection::open(dir.join("meta.db")).expect("open");
    conn.execute_batch(SCHEMA_V1).expect("v1 schema");
    conn.pragma_update(None, "user_version", 1u32)
        .expect("version");
    conn.execute_batch(extra).expect("seed");
}

fn open(dir: &std::path::Path) -> mailbox_core::error::Result<Store> {
    Store::open(dir, "test", Tracer::disabled(), Clock::fixed(1_788_264_000))
}

const ONE_MESSAGE: &str = "
    INSERT INTO folders (id, path, name, special_use, rank)
        VALUES (1, 'INBOX', 'INBOX', 'inbox', 0);
    INSERT INTO messages (rowid, id, date, subject, flags, primary_folder)
        VALUES (1, 'aa11bb22cc33dd44', 1788264000, 'Hello', ' ', 1);
    INSERT INTO placements (folder_id, uid, message_rowid, flags)
        VALUES (1, 5, 1, ' ');
";

#[test]
fn a_v1_store_upgrades_to_the_current_schema() {
    let dir = tempdir::TempDir::new();
    v1_store(dir.path(), ONE_MESSAGE);

    let store = open(dir.path()).expect("a v1 store should upgrade");
    assert_eq!(store.message_count().unwrap(), 1);
    assert_eq!(store.folder_count().unwrap(), 1);

    // Every table the current build expects is there. Before the boundaries
    // were fixed, `addresses` was created by the *third* migration, so
    // upgrading a real v1 store died on "table addresses already exists".
    let conn = rusqlite::Connection::open(dir.path().join("meta.db")).unwrap();
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, mailbox_core::store::schema::SCHEMA_VERSION);

    for table in [
        "folders",
        "messages",
        "message_refs",
        "placements",
        "addresses",
        "attachments",
        "tags",
        "contacts",
        "ops",
        "outbox",
        "messages_fts",
    ] {
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "`{table}` is missing after the upgrade");
    }
}

/// v1 keyed placements on (folder_id, uid), so one message could sit in one
/// folder twice under different UIDs — a re-`APPEND`, or a rescan after a
/// `UIDVALIDITY` change, produced exactly that. The new key forbids it, so the
/// migration has to collapse those rows rather than fail on them.
#[test]
fn duplicate_placements_are_collapsed_keeping_the_newest_uid() {
    let dir = tempdir::TempDir::new();
    v1_store(
        dir.path(),
        "
        INSERT INTO folders (id, path, name, special_use, rank)
            VALUES (1, 'INBOX', 'INBOX', 'inbox', 0);
        INSERT INTO messages (rowid, id, date, subject, flags, primary_folder)
            VALUES (1, 'aa11bb22cc33dd44', 1788264000, 'Hello', ' ', 1);
        INSERT INTO placements (folder_id, uid, message_rowid, flags) VALUES (1, 5, 1, ' ');
        INSERT INTO placements (folder_id, uid, message_rowid, flags) VALUES (1, 9, 1, ' ');
        INSERT INTO placements (folder_id, uid, message_rowid, flags) VALUES (1, 7, 1, ' ');
        ",
    );

    let store = open(dir.path()).expect("duplicates must not fail the upgrade");
    assert_eq!(store.message_count().unwrap(), 1);

    let uids = store.placement_uids(1).unwrap();
    assert_eq!(
        uids.into_iter().collect::<Vec<_>>(),
        vec![9],
        "the highest UID is the most recent thing the server said; the rest are stale"
    );
}

/// Several messages, several folders, and duplicates in more than one place —
/// the deduplication has to be per (folder, message), not global.
#[test]
fn deduplication_is_per_folder_and_message() {
    let dir = tempdir::TempDir::new();
    v1_store(
        dir.path(),
        "
        INSERT INTO folders (id, path, name, special_use, rank)
            VALUES (1, 'INBOX', 'INBOX', 'inbox', 0), (2, 'Archive', 'Archive', 'archive', 1);
        INSERT INTO messages (rowid, id, date, subject, flags, primary_folder)
            VALUES (1, 'aa11', 1788264000, 'One', ' ', 1),
                   (2, 'bb22', 1788264001, 'Two', ' ', 1);
        INSERT INTO placements (folder_id, uid, message_rowid, flags) VALUES
            (1, 1, 1, ' '), (1, 4, 1, ' '),
            (1, 2, 2, ' '),
            (2, 8, 1, ' '), (2, 3, 1, ' ');
        ",
    );

    let store = open(dir.path()).expect("upgrade");
    // INBOX keeps message 1 at uid 4 and message 2 at uid 2.
    assert_eq!(
        store
            .placement_uids(1)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![2, 4]
    );
    // Archive keeps message 1 at uid 8 — the same message, a different folder,
    // its own winner.
    assert_eq!(
        store
            .placement_uids(2)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        vec![8]
    );
    assert_eq!(store.message_count().unwrap(), 2);
}

/// The upgrade must be usable, not merely successful: reads work afterwards and
/// the derived state is intact.
#[test]
fn an_upgraded_store_is_readable() {
    let dir = tempdir::TempDir::new();
    v1_store(dir.path(), ONE_MESSAGE);
    open(dir.path()).expect("upgrade");

    // A read-only open is the path every listing takes (§4.1), and before the
    // upgrade it refused outright.
    let store = Store::open_read_only(
        dir.path(),
        "test",
        Tracer::disabled(),
        Clock::fixed(1_788_264_000),
    )
    .expect("a read-only open should work once the store is current");

    let node = mailbox_core::query::parse("", &Clock::fixed(0)).unwrap();
    let listing = store
        .search(&node, &mailbox_core::store::read::ListOptions::default())
        .expect("search");
    assert_eq!(listing.messages.len(), 1);
    assert_eq!(listing.messages[0].subject, "Hello");
    assert_eq!(listing.messages[0].folder, "INBOX");
}

/// A read never migrates (§4.1), and the refusal has to name the command that
/// does — "run a writing command" sends the reader looking for one.
#[test]
fn a_read_of_an_old_store_names_the_command_that_upgrades_it() {
    let dir = tempdir::TempDir::new();
    v1_store(dir.path(), ONE_MESSAGE);

    let err = Store::open_read_only(
        dir.path(),
        "test",
        Tracer::disabled(),
        Clock::fixed(1_788_264_000),
    )
    .expect_err("a read must not migrate");

    let message = err.to_string();
    assert!(
        message.contains("mailbox sync"),
        "the refusal must name the command: {message}"
    );
    assert!(
        message.contains("v1"),
        "and which version it found: {message}"
    );
    // No stray whitespace from a wrapped source string.
    assert!(
        !message.contains("  "),
        "the message has collapsed whitespace: {message:?}"
    );
}

/// A store from a *newer* build cannot be repaired by migrating, so the advice
/// has to be different.
#[test]
fn a_store_from_the_future_says_so() {
    let dir = tempdir::TempDir::new();
    v1_store(dir.path(), ONE_MESSAGE);
    {
        let conn = rusqlite::Connection::open(dir.path().join("meta.db")).unwrap();
        conn.pragma_update(None, "user_version", 99u32).unwrap();
    }

    let err = open(dir.path()).expect_err("a future store must not be opened");
    let message = err.to_string();
    assert!(message.contains("newer mailbox"), "{message}");
    assert!(
        message.contains("upgrade mailbox") || message.contains("delete the store"),
        "the advice must be actionable: {message}"
    );
    assert!(!message.contains("  "), "collapsed whitespace: {message:?}");
}

/// Migrating twice is a no-op: opening an already-current store must not try to
/// re-run anything.
#[test]
fn upgrading_is_idempotent() {
    let dir = tempdir::TempDir::new();
    v1_store(dir.path(), ONE_MESSAGE);

    open(dir.path()).expect("first upgrade");
    open(dir.path()).expect("second open");
    let store = open(dir.path()).expect("third open");
    assert_eq!(store.message_count().unwrap(), 1);
}

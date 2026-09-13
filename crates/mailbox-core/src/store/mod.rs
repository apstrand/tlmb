//! The local store (§5.1): `meta.db`, the blob CAS, and `state.toml`, under
//! `$XDG_DATA_HOME/mailbox/<account-id>/`.
//!
//! One store holds one account. That is not an arbitrary limit — §4.1 makes
//! exactly one process the sync engine *per account*, and a per-account store
//! is what lets a second account be owned by a different process without any
//! shared locking.
//!
//! Read-only opens skip ownership entirely (§4.1): a script or an agent that
//! only lists mail takes the WAL read path and never contends with the TUI.

pub mod blobs;
pub mod changes;
pub mod corpus;
pub mod ingest;
pub mod ops;
pub mod outbox;
pub mod read;
pub mod schema;
pub mod snooze;
pub mod state;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::storage::{ByteStorage, FsStorage, MemoryStorage};
use crate::trace::{event, Tracer};

pub use blobs::Blobs;
pub use changes::{ChangeKind, ChangePage};
pub use corpus::{CorpusReport, CorpusSpec};
pub use ops::{FlagChange, Op, OpKind, OpState};
pub use outbox::{NewOutboxEntry, OutboxEntry, OutboxState};
pub use state::{FolderState, SyncState};
pub use write::{write_result, Written};

/// How a folder is ranked when a message is filed in several of them (§5.3).
pub mod rank {
    pub const INBOX: i64 = 0;
    pub const SPECIAL: i64 = 1;
    pub const NORMAL: i64 = 2;
    /// `[Gmail]/All Mail` and friends: real folders, but never the one a
    /// message should be *reported* as living in when a better one exists.
    pub const CATCH_ALL: i64 = 3;
}

/// A folder as the sync engine discovered it.
#[derive(Debug, Clone, Default)]
pub struct FolderRecord {
    pub path: String,
    pub name: String,
    pub delimiter: Option<String>,
    pub special_use: Option<String>,
    pub selectable: bool,
    pub subscribed: bool,
}

impl FolderRecord {
    pub fn rank(&self) -> i64 {
        match self.special_use.as_deref() {
            Some("inbox") => rank::INBOX,
            Some("all") => rank::CATCH_ALL,
            Some(_) => rank::SPECIAL,
            None => rank::NORMAL,
        }
    }
}

/// The `Debug` impl deliberately shows the shape rather than the contents: a
/// store's contents are the user's mail, and a stray `{store:?}` in a log or a
/// test failure should not print it.
pub struct Store {
    dir: PathBuf,
    account: String,
    conn: Connection,
    blobs: Blobs,
    storage: Arc<dyn ByteStorage>,
    tracer: Tracer,
    clock: Clock,
    read_only: bool,
}

impl Store {
    /// Opens the real mailbox schema in memory.
    ///
    /// This is intentionally a narrow constructor for targets without a host
    /// filesystem. It lets the wasm target exercise the production schema;
    /// persistent browser callers should restore and checkpoint serialized
    /// images with [`Store::open_serialized`] and [`Store::serialize`].
    pub fn open_in_memory(account: &str, tracer: Tracer, clock: Clock) -> Result<Store> {
        Self::open_serialized(None, account, tracer, clock)
    }

    /// Restores an in-memory store from SQLite's portable serialized image.
    /// Browser code reads/writes this image through OPFS; callers above Store
    /// remain unaware of where the bytes live.
    pub fn open_serialized(
        database: Option<&[u8]>,
        account: &str,
        tracer: Tracer,
        clock: Clock,
    ) -> Result<Store> {
        Self::open_serialized_with_storage(
            database,
            Arc::new(MemoryStorage::new()),
            account,
            tracer,
            clock,
        )
    }

    pub fn open_serialized_with_storage(
        database: Option<&[u8]>,
        storage: Arc<dyn ByteStorage>,
        account: &str,
        tracer: Tracer,
        clock: Clock,
    ) -> Result<Store> {
        let mut conn = Connection::open_in_memory()?;
        if let Some(database) = database.filter(|bytes| !bytes.is_empty()) {
            conn.deserialize_read_exact(
                rusqlite::MAIN_DB,
                std::io::Cursor::new(database),
                database.len(),
                false,
            )?;
        }
        schema::migrate(&mut conn, &tracer)?;
        configure(&conn)?;

        Ok(Store {
            blobs: Blobs::with_storage(storage.clone(), "blobs"),
            storage,
            dir: PathBuf::new(),
            account: account.to_string(),
            conn,
            tracer,
            clock,
            read_only: false,
        })
    }

    /// Takes a consistent SQLite image suitable for an atomic OPFS write.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        Ok(self.conn.serialize(rusqlite::MAIN_DB)?.to_vec())
    }

    /// Opens a store for writing, creating it if absent.
    pub fn open(
        dir: impl AsRef<Path>,
        account: &str,
        tracer: Tracer,
        clock: Clock,
    ) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        // §9: the store is unencrypted by default, so its permissions are the
        // protection. 0700 on the directory, not just on the files, so a
        // filename cannot leak a correspondent's address.
        restrict_permissions(&dir)?;
        std::fs::create_dir_all(dir.join("blobs"))?;

        let mut conn = Connection::open(dir.join("meta.db"))?;
        schema::migrate(&mut conn, &tracer)?;
        configure(&conn)?;
        let storage: Arc<dyn ByteStorage> = Arc::new(FsStorage::new(&dir));

        Ok(Store {
            blobs: Blobs::with_storage(storage.clone(), "blobs"),
            storage,
            dir,
            account: account.to_string(),
            conn,
            tracer,
            clock,
            read_only: false,
        })
    }

    /// The fast path of §4.1: a reader neither takes the owner lock nor blocks
    /// on the writer, because WAL readers do not.
    pub fn open_read_only(
        dir: impl AsRef<Path>,
        account: &str,
        tracer: Tracer,
        clock: Clock,
    ) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        let db = dir.join("meta.db");

        // An account that has never synced has no store, and that is a normal
        // state rather than an error: `ls` on it should say "no messages", and
        // `accounts test` should not need a store at all. An empty in-memory
        // database gives every read the honest answer without creating
        // anything on disk — which a read must not do (§4.1).
        if !db.exists() {
            let mut conn = Connection::open_in_memory()?;
            schema::migrate(&mut conn, &tracer)?;
            let storage: Arc<dyn ByteStorage> = Arc::new(FsStorage::new(&dir));
            return Ok(Store {
                blobs: Blobs::with_storage(storage.clone(), "blobs"),
                storage,
                dir,
                account: account.to_string(),
                conn,
                tracer,
                clock,
                read_only: true,
            });
        }

        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // A read never migrates: §4.1 makes reads the path that skips ownership
        // entirely, and a read that writes would need it. So this refuses — but
        // it has to say precisely what to run, because "a writing command" sends
        // the reader looking for one.
        let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < schema::SCHEMA_VERSION {
            return Err(Error::store(format!(
                concat!(
                    "this store is from an older mailbox (schema v{}, this build ",
                    "speaks v{}). Run `mailbox sync` once to upgrade it — reads do ",
                    "not migrate, so that a read never needs to become a writer."
                ),
                version,
                schema::SCHEMA_VERSION
            )));
        }
        if version > schema::SCHEMA_VERSION {
            return Err(Error::store(format!(
                concat!(
                    "this store was written by a newer mailbox (schema v{}, this ",
                    "build speaks v{}); upgrade mailbox, or delete the store and ",
                    "re-sync"
                ),
                version,
                schema::SCHEMA_VERSION
            )));
        }
        configure(&conn)?;
        let storage: Arc<dyn ByteStorage> = Arc::new(FsStorage::new(&dir));
        Ok(Store {
            blobs: Blobs::with_storage(storage.clone(), "blobs"),
            storage,
            dir,
            account: account.to_string(),
            conn,
            tracer,
            clock,
            read_only: true,
        })
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn blobs(&self) -> &Blobs {
        &self.blobs
    }

    pub fn tracer(&self) -> &Tracer {
        &self.tracer
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn require_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::store("this store was opened read-only"));
        }
        Ok(())
    }

    /// Runs `f` inside a transaction, tracing both boundaries.
    ///
    /// Every write goes through here. The trace pair is what lets the
    /// crash-only oracle (§14.3) tell "we died before the commit" from "we died
    /// after it" without guessing.
    pub fn transaction<T>(
        &mut self,
        label: &str,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.require_writable()?;
        self.tracer.emit1(event::STORE_TXN_BEGIN, "label", label);
        let tx = self.conn.transaction()?;
        match f(&tx) {
            Ok(v) => {
                tx.commit()?;
                self.tracer.emit1(event::STORE_TXN_COMMIT, "label", label);
                Ok(v)
            }
            Err(e) => {
                // The rollback is implicit in the drop; the trace is not.
                self.tracer.emit(
                    event::STORE_TXN_ROLLBACK,
                    serde_json::json!({ "label": label, "error": e.to_string() }),
                );
                Err(e)
            }
        }
    }

    // ---- folders -------------------------------------------------------

    /// Inserts or updates a folder, returning its local id.
    pub fn upsert_folder(&mut self, rec: &FolderRecord) -> Result<i64> {
        self.require_writable()?;
        let rank = rec.rank();
        self.conn.execute(
            "INSERT INTO folders (path, name, delimiter, special_use, selectable, rank, subscribed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                name        = excluded.name,
                delimiter   = excluded.delimiter,
                special_use = excluded.special_use,
                selectable  = excluded.selectable,
                rank        = excluded.rank,
                subscribed  = excluded.subscribed",
            rusqlite::params![
                rec.path,
                rec.name,
                rec.delimiter,
                rec.special_use,
                rec.selectable as i64,
                rank,
                rec.subscribed as i64,
            ],
        )?;
        self.folder_id(&rec.path)?
            .ok_or_else(|| Error::store(format!("folder {} vanished after upsert", rec.path)))
    }

    pub fn folder_id(&self, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM folders WHERE path = ?1", [path], |r| {
                r.get(0)
            })
            .ok())
    }

    /// Drops a folder and every placement in it. Messages themselves survive:
    /// the same message may be placed in another folder, and even when it is
    /// not, [`Store::gc_orphans`] is what decides its fate.
    pub fn delete_folder(&mut self, path: &str) -> Result<()> {
        self.require_writable()?;
        self.conn
            .execute("DELETE FROM folders WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Recomputes `folders.total` / `folders.unread` from the placements.
    ///
    /// Derived rather than incremented, because an incremented counter that
    /// drifts is a bug nobody notices until a user counts by hand.
    pub fn refresh_folder_counts(&mut self) -> Result<()> {
        self.require_writable()?;
        self.conn.execute_batch(
            "UPDATE folders SET
                total = (SELECT count(*) FROM placements p WHERE p.folder_id = folders.id),
                unread = (SELECT count(*) FROM placements p
                          WHERE p.folder_id = folders.id AND p.seen = 0)",
        )?;
        Ok(())
    }

    /// Deletes messages no folder places any more, and the blobs that only they
    /// referenced.
    pub fn gc_orphans(&mut self) -> Result<u64> {
        self.require_writable()?;
        let orphan_blobs: Vec<String> = {
            let mut st = self.conn.prepare(
                "SELECT DISTINCT m.blob_id FROM messages m
                 WHERE m.blob_id IS NOT NULL
                   AND NOT EXISTS (SELECT 1 FROM placements p WHERE p.message_rowid = m.rowid)",
            )?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.filter_map(std::result::Result::ok).collect()
        };
        let removed = self.conn.execute(
            "DELETE FROM messages WHERE NOT EXISTS
                (SELECT 1 FROM placements p WHERE p.message_rowid = messages.rowid)",
            [],
        )?;
        for blob in orphan_blobs {
            // A blob is shared by content address, so only delete it once no
            // surviving row names it.
            let still: i64 = self.conn.query_row(
                "SELECT count(*) FROM messages WHERE blob_id = ?1",
                [&blob],
                |r| r.get(0),
            )?;
            if still == 0 {
                self.blobs.remove(&blob)?;
            }
        }
        Ok(removed as u64)
    }

    /// `meta.db` plus WAL plus blobs, for `mailbox status`.
    pub fn disk_usage(&self) -> u64 {
        let mut total = self.blobs.disk_usage();
        for name in ["meta.db", "meta.db-wal", "meta.db-shm", "state.toml"] {
            total += self.storage.usage(Path::new(name));
        }
        total
    }

    pub fn sync_state(&self) -> SyncState {
        SyncState::load_from(self.storage.as_ref(), Path::new("state.toml"))
    }

    pub fn save_sync_state(&self, s: &SyncState) -> Result<()> {
        s.save_to(self.storage.as_ref(), Path::new("state.toml"))
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("account", &self.account)
            .field("dir", &self.dir)
            .field("read_only", &self.read_only)
            .finish()
    }
}

fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Wait rather than fail when the owner is mid-commit; §4.1 makes contention
    // rare, and a `database is locked` error would surface to a user as a
    // mysterious failure of a read.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(dir)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) -> Result<()> {
    Ok(())
}

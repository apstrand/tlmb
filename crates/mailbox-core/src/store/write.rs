//! Optimistic local writes (§5.3).
//!
//! Every function here does two things in one transaction: it changes what the
//! user sees, and it queues the operation that will make the server agree. The
//! atomicity is the point — there is no window in which the mailbox shows an
//! edit that nothing will push, and none in which an operation exists for an
//! edit that was rolled back.
//!
//! The server is not consulted. That is what makes `mailbox flag` instant, and
//! what makes it work on a train.

use rusqlite::{params, Transaction};

use crate::error::{Error, Result};
use crate::flags;
use crate::store::changes::{self, ChangeKind, Event};
use crate::store::ops::{FlagChange, OpKind};
use crate::store::Store;

/// What a local write did, for the caller to report.
#[derive(Debug, Clone)]
pub struct Written {
    pub message_id: String,
    /// The flags, or the folder, after the change.
    pub flags: Vec<String>,
    pub folder: String,
    /// False when the change was a no-op — the flag was already set, the
    /// message was already in that folder — in which case nothing was queued.
    pub changed: bool,
}

impl Store {
    /// Adds and removes flags on a message (§6.1's `flag`).
    ///
    /// Flags are applied to the message's *primary* placement, which is the one
    /// every listing reports (§5.3's folder ranking). A message carrying two
    /// Gmail labels is one message with one read state, and pushing the change
    /// to the folder the user was looking at is what they meant.
    pub fn set_flags(
        &mut self,
        message_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<Written> {
        let now = self.clock.now();
        let state = self.sync_state();
        self.transaction("set_flags", |tx| {
            let p = primary_placement(tx, message_id)?;
            let (rowid, folder_id, folder_path, current, uid) =
                (p.rowid, p.folder_id, p.folder_path, p.flags, p.uid);

            let mut set: Vec<String> = flags::unpack(&current);
            let before = set.clone();
            for f in remove {
                set.retain(|x| x != f);
            }
            for f in add {
                if !set.contains(f) {
                    set.push(f.clone());
                }
            }
            let packed = flags::pack(&set);

            if set_eq(&before, &set) {
                return Ok(Written {
                    message_id: message_id.to_string(),
                    flags: flags::unpack(&packed),
                    folder: folder_path,
                    changed: false,
                });
            }

            tx.execute(
                "UPDATE placements SET flags = ?2 WHERE folder_id = ?1 AND message_rowid = ?3",
                params![folder_id, packed, rowid],
            )?;
            tx.execute(
                "UPDATE messages SET flags = ?2 WHERE rowid = ?1",
                params![rowid, packed],
            )?;

            // Only the flags that actually moved are queued. Pushing a `STORE`
            // for a flag that was already set is a write the server did not
            // need and a chance for it to disagree.
            let change = FlagChange {
                add: add
                    .iter()
                    .filter(|f| !before.contains(f))
                    .cloned()
                    .collect(),
                remove: remove
                    .iter()
                    .filter(|f| before.contains(f))
                    .cloned()
                    .collect(),
            };
            if !change.is_empty() {
                Store::enqueue_op_in(
                    tx,
                    now,
                    OpKind::Flag,
                    message_id,
                    Some(&folder_path),
                    None,
                    uid,
                    Some(state.folder(&folder_path).uidvalidity),
                    &change,
                )?;
            }

            changes::record(
                tx,
                now,
                &Event::new(ChangeKind::Flag, message_id)
                    .local()
                    .in_folder(folder_path.as_str())
                    .with(serde_json::json!({ "flags": flags::unpack(&packed) })),
            )?;

            Ok(Written {
                message_id: message_id.to_string(),
                flags: flags::unpack(&packed),
                folder: folder_path,
                changed: true,
            })
        })
    }

    /// Moves a message to another folder (§6.1's `mv`).
    ///
    /// The destination placement is created without a UID: the message is there
    /// *now*, and the server names it when the operation is replayed or the
    /// next sync of that folder finds it.
    pub fn move_message(&mut self, message_id: &str, target: &str) -> Result<Written> {
        let now = self.clock.now();
        let state = self.sync_state();
        self.transaction("move_message", |tx| {
            let p = primary_placement(tx, message_id)?;
            let (rowid, folder_id, folder_path, current, uid) =
                (p.rowid, p.folder_id, p.folder_path, p.flags, p.uid);
            let (target_id, target_path) = folder_by_name(tx, target)?;

            if target_id == folder_id {
                return Ok(Written {
                    message_id: message_id.to_string(),
                    flags: flags::unpack(&current),
                    folder: target_path,
                    changed: false,
                });
            }

            tx.execute(
                "DELETE FROM placements WHERE folder_id = ?1 AND message_rowid = ?2",
                params![folder_id, rowid],
            )?;
            upsert_pending_placement(tx, target_id, rowid, &current)?;
            super::ingest::refresh_primary(tx, rowid)?;

            Store::enqueue_op_in(
                tx,
                now,
                OpKind::Move,
                message_id,
                Some(&folder_path),
                Some(&target_path),
                // Recorded before the local move erases the source placement:
                // by replay time the store no longer knows what the server
                // calls this message in the folder it is leaving. The
                // UIDVALIDITY is what says whether that record is still true.
                uid,
                Some(state.folder(&folder_path).uidvalidity),
                &FlagChange::default(),
            )?;

            changes::record(
                tx,
                now,
                &Event::new(ChangeKind::Move, message_id)
                    .local()
                    .in_folder(target_path.as_str())
                    .with(serde_json::json!({ "from": folder_path })),
            )?;

            Ok(Written {
                message_id: message_id.to_string(),
                flags: flags::unpack(&current),
                folder: target_path,
                changed: true,
            })
        })
    }

    /// Copies a message into another folder (§6.1's `cp`), leaving the original.
    pub fn copy_message(&mut self, message_id: &str, target: &str) -> Result<Written> {
        let now = self.clock.now();
        let state = self.sync_state();
        self.transaction("copy_message", |tx| {
            let p = primary_placement(tx, message_id)?;
            let (rowid, folder_path, current, uid) = (p.rowid, p.folder_path, p.flags, p.uid);
            let (target_id, target_path) = folder_by_name(tx, target)?;

            let already: i64 = tx.query_row(
                "SELECT count(*) FROM placements WHERE folder_id = ?1 AND message_rowid = ?2",
                params![target_id, rowid],
                |r| r.get(0),
            )?;
            if already > 0 {
                return Ok(Written {
                    message_id: message_id.to_string(),
                    flags: flags::unpack(&current),
                    folder: target_path,
                    changed: false,
                });
            }

            upsert_pending_placement(tx, target_id, rowid, &current)?;
            super::ingest::refresh_primary(tx, rowid)?;

            Store::enqueue_op_in(
                tx,
                now,
                OpKind::Copy,
                message_id,
                Some(&folder_path),
                Some(&target_path),
                // Recorded before the local move erases the source placement:
                // by replay time the store no longer knows what the server
                // calls this message in the folder it is leaving. The
                // UIDVALIDITY is what says whether that record is still true.
                uid,
                Some(state.folder(&folder_path).uidvalidity),
                &FlagChange::default(),
            )?;

            changes::record(
                tx,
                now,
                &Event::new(ChangeKind::Copy, message_id)
                    .local()
                    .in_folder(target_path.as_str()),
            )?;

            Ok(Written {
                message_id: message_id.to_string(),
                flags: flags::unpack(&current),
                // A copy leaves the message where it was; the destination is
                // an addition, not a relocation.
                folder: folder_path,
                changed: true,
            })
        })
    }

    /// Adds and removes local tags (§5.4).
    ///
    /// Nothing is queued: tags are local until M8 gives them the state folder,
    /// and IMAP has nowhere to put them. Keywords a *server* understands are
    /// flags, and go through [`Store::set_flags`].
    pub fn set_tags(
        &mut self,
        message_id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<Vec<String>> {
        let now = self.clock.now();
        self.transaction("set_tags", |tx| {
            let rowid: i64 = tx
                .query_row(
                    "SELECT rowid FROM messages WHERE id = ?1",
                    [message_id],
                    |r| r.get(0),
                )
                .map_err(|_| Error::not_found("message", message_id))?;

            for tag in remove {
                tx.execute(
                    "DELETE FROM tags WHERE message_rowid = ?1 AND tag = ?2",
                    params![rowid, tag.to_ascii_lowercase()],
                )?;
            }
            for tag in add {
                tx.execute(
                    "INSERT OR IGNORE INTO tags (message_rowid, tag) VALUES (?1, ?2)",
                    params![rowid, tag.to_ascii_lowercase()],
                )?;
            }

            let mut st =
                tx.prepare("SELECT tag FROM tags WHERE message_rowid = ?1 ORDER BY tag")?;
            let rows = st.query_map([rowid], |r| r.get::<_, String>(0))?;
            let tags: Vec<String> = rows.filter_map(std::result::Result::ok).collect();

            if !add.is_empty() || !remove.is_empty() {
                changes::record(
                    tx,
                    now,
                    &Event::new(ChangeKind::Tag, message_id)
                        .local()
                        .with(serde_json::json!({ "tags": tags })),
                )?;
            }
            Ok(tags)
        })
    }

    /// Marks messages as announced (§11), so they are not announced twice.
    pub fn mark_notified(&mut self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.transaction("mark_notified", |tx| {
            let mut st = tx.prepare("UPDATE messages SET notified = 1 WHERE id = ?1")?;
            for id in ids {
                st.execute([id])?;
            }
            Ok(())
        })
    }

    /// Marks everything as already announced.
    ///
    /// Used after a first sync: a mailbox that has just been added should not
    /// produce a notification for every message it has ever received.
    pub fn mark_all_notified(&mut self) -> Result<u64> {
        self.transaction("mark_all_notified", |tx| {
            let n = tx.execute("UPDATE messages SET notified = 1 WHERE notified = 0", [])?;
            Ok(n as u64)
        })
    }

    /// Records the UID the server assigned to a placement created locally.
    pub fn confirm_placement(&mut self, message_id: &str, folder: &str, uid: u32) -> Result<()> {
        self.transaction("confirm_placement", |tx| {
            let (target_id, _) = folder_by_name(tx, folder)?;
            let rowid: i64 = tx
                .query_row(
                    "SELECT rowid FROM messages WHERE id = ?1",
                    [message_id],
                    |r| r.get(0),
                )
                .map_err(|_| Error::not_found("message", message_id))?;
            tx.execute(
                "UPDATE placements SET uid = ?3 WHERE folder_id = ?1 AND message_rowid = ?2",
                params![target_id, rowid, uid],
            )?;
            Ok(())
        })
    }

    /// The folder a message is reported as living in, and its UID there.
    ///
    /// `None` for the UID means the placement is still local-only. Used by the
    /// replay to turn a stable id into something the wire can name.
    pub fn locate(&self, message_id: &str, folder: &str) -> Result<Option<u32>> {
        let uid: Option<Option<u32>> = self
            .conn
            .query_row(
                "SELECT p.uid FROM placements p
                 JOIN messages m ON m.rowid = p.message_rowid
                 JOIN folders f ON f.id = p.folder_id
                 WHERE m.id = ?1 AND f.path = ?2",
                params![message_id, folder],
                |r| r.get(0),
            )
            .ok();
        Ok(uid.flatten())
    }
}

/// The placement every listing reports for a message (§5.3's folder ranking).
#[derive(Debug, Clone)]
struct Primary {
    rowid: i64,
    folder_id: i64,
    folder_path: String,
    /// Packed flags, as stored.
    flags: String,
    /// What the server calls it there. `None` while a local move is still
    /// waiting to be pushed.
    uid: Option<u32>,
}

fn primary_placement(tx: &Transaction<'_>, message_id: &str) -> Result<Primary> {
    tx.query_row(
        "SELECT m.rowid, p.folder_id, f.path, p.flags, p.uid
         FROM messages m
         JOIN placements p ON p.folder_id = m.primary_folder AND p.message_rowid = m.rowid
         JOIN folders f ON f.id = p.folder_id
         WHERE m.id = ?1",
        [message_id],
        |r| {
            Ok(Primary {
                rowid: r.get(0)?,
                folder_id: r.get(1)?,
                folder_path: r.get(2)?,
                flags: r.get(3)?,
                uid: r.get(4)?,
            })
        },
    )
    .map_err(|_| Error::not_found("message", message_id))
}

/// Resolves a folder by path, leaf name, or special-use — the same three ways
/// `in:` accepts (§6.2), so `mv 1 archive` works without knowing what the
/// server calls it.
fn folder_by_name(tx: &Transaction<'_>, name: &str) -> Result<(i64, String)> {
    tx.query_row(
        "SELECT id, path FROM folders
         WHERE path = ?1 COLLATE NOCASE OR name = ?1 COLLATE NOCASE OR special_use = ?2
         ORDER BY rank, id LIMIT 1",
        params![name, name.to_ascii_lowercase()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(|_| Error::not_found("folder", name))
}

fn upsert_pending_placement(
    tx: &Transaction<'_>,
    folder_id: i64,
    rowid: i64,
    flags: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO placements (folder_id, uid, message_rowid, flags)
         VALUES (?1, NULL, ?2, ?3)
         ON CONFLICT(folder_id, message_rowid) DO UPDATE SET flags = excluded.flags",
        params![folder_id, rowid, flags],
    )?;
    Ok(())
}

fn set_eq(a: &[String], b: &[String]) -> bool {
    let mut a: Vec<&str> = a.iter().map(String::as_str).collect();
    let mut b: Vec<&str> = b.iter().map(String::as_str).collect();
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

//! The operation queue of §5.3.
//!
//! > Writes are optimistic: local state changes immediately, the operation is
//! > queued, and the queue is replayed with idempotent semantics on reconnect.
//! > Conflicts resolve to the server's flags with a note in `mailbox status`.
//!
//! Three properties do the work:
//!
//! 1. **An operation names a message by its stable id (§5.2) and a folder by
//!    its path — never by UID.** The UID is resolved when the operation is
//!    replayed, so a queue that outlives a `UIDVALIDITY` reset, a reconnect, or
//!    a week offline still means what it meant when it was written.
//! 2. **Every operation is expressed as a desired state, not as a delta on the
//!    wire.** `flag` says "add `seen`, remove `flagged`", which is what
//!    `UID STORE +FLAGS`/`-FLAGS` already are: replaying it twice is a no-op.
//! 3. **The queue is in `meta.db`,** so it commits with the local change it
//!    describes. There is no window in which the user's mailbox shows an edit
//!    that no operation will ever push, or an operation for an edit that was
//!    rolled back.

use rusqlite::{params, Transaction};
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::error::Result;
use crate::store::Store;

/// What kind of write an operation performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// `UID STORE` — add and remove flags.
    Flag,
    /// `UID MOVE`, or `UID COPY` + `\Deleted` + `EXPUNGE` where `MOVE` is not
    /// advertised (§5.3).
    Move,
    /// `UID COPY`.
    Copy,
    /// `\Deleted` + `EXPUNGE`. Not reachable from the CLI — `rm` is a move to
    /// Trash (§6.1) — but it is what the `MOVE` fallback is built from.
    Delete,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Flag => "flag",
            OpKind::Move => "move",
            OpKind::Copy => "copy",
            OpKind::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<OpKind> {
        match s {
            "flag" => Some(OpKind::Flag),
            "move" => Some(OpKind::Move),
            "copy" => Some(OpKind::Copy),
            "delete" => Some(OpKind::Delete),
            _ => None,
        }
    }
}

/// The `flag` payload: which flags to add and which to remove.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlagChange {
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

impl FlagChange {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    Pending,
    Done,
    /// Gave up. The local state is now knowingly ahead of the server, and the
    /// next pull will overwrite it — which is §5.3's "conflicts resolve to the
    /// server's flags", with the note surfacing in `mailbox status`.
    Failed,
}

impl OpState {
    pub fn as_str(self) -> &'static str {
        match self {
            OpState::Pending => "pending",
            OpState::Done => "done",
            OpState::Failed => "failed",
        }
    }
}

/// One queued write.
#[derive(Debug, Clone)]
pub struct Op {
    pub id: i64,
    pub kind: OpKind,
    /// The stable id of §5.2.
    pub message_id: String,
    /// The folder the message was in when this was queued.
    pub folder_path: Option<String>,
    /// Destination, for `move` and `copy`.
    pub target_path: Option<String>,
    /// The UID the message had in `folder_path` when this was queued, and the
    /// `UIDVALIDITY` that UID is only meaningful under.
    pub source_uid: Option<u32>,
    pub source_uidvalidity: Option<u32>,
    pub flags: FlagChange,
    pub state: OpState,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created: Timestamp,
    pub next_attempt: Timestamp,
}

/// How long to wait before retrying an operation that failed transiently.
///
/// Doubling from 15 s, capped at an hour. The cap matters more than the curve:
/// a laptop that was closed for a week must not come back and wait another week
/// before trying again.
pub fn backoff_seconds(attempts: u32) -> i64 {
    const BASE: i64 = 15;
    const CAP: i64 = 3600;
    BASE.saturating_mul(1i64 << attempts.min(8)).min(CAP)
}

/// After this many failures an operation is abandoned and becomes a conflict.
///
/// Retrying forever would keep the mailbox permanently disagreeing with the
/// server about a message that, say, no longer exists. Giving up and letting
/// the next pull win is the honest resolution.
pub const MAX_ATTEMPTS: u32 = 8;

impl Store {
    /// Queues a write. Call inside the same transaction as the local change.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_op_in(
        tx: &Transaction<'_>,
        now: Timestamp,
        kind: OpKind,
        message_id: &str,
        folder_path: Option<&str>,
        target_path: Option<&str>,
        source_uid: Option<u32>,
        source_uidvalidity: Option<u32>,
        flags: &FlagChange,
    ) -> Result<i64> {
        // Flag operations against the same message and folder collapse: a user
        // toggling a flag five times should produce one `UID STORE`, not five,
        // and the last write is the one that means anything.
        if kind == OpKind::Flag {
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, payload FROM ops
                     WHERE state = 'pending' AND kind = 'flag'
                       AND message_id = ?1 AND folder_path IS ?2
                     ORDER BY id DESC LIMIT 1",
                    params![message_id, folder_path],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            if let Some((id, payload)) = existing {
                let mut merged: FlagChange = serde_json::from_str(&payload).unwrap_or_default();
                merge_flags(&mut merged, flags);
                tx.execute(
                    "UPDATE ops SET payload = ?2, next_attempt = 0 WHERE id = ?1",
                    params![id, serde_json::to_string(&merged)?],
                )?;
                return Ok(id);
            }
        }

        tx.execute(
            "INSERT INTO ops
                (kind, message_id, folder_path, target_path,
                 source_uid, source_uidvalidity, payload, created)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                kind.as_str(),
                message_id,
                folder_path,
                target_path,
                source_uid,
                source_uidvalidity,
                serde_json::to_string(flags)?,
                now,
            ],
        )?;
        Ok(tx.last_insert_rowid())
    }

    /// Operations ready to replay, oldest first.
    ///
    /// Order is the contract: two flag changes to one message must reach the
    /// server in the order the user made them, and a move queued after a flag
    /// must not overtake it.
    pub fn pending_ops(&self, now: Timestamp, limit: usize) -> Result<Vec<Op>> {
        let mut st = self.conn.prepare(
            "SELECT id, kind, message_id, folder_path, target_path, payload, state,
                    attempts, last_error, created, next_attempt,
                    source_uid, source_uidvalidity
             FROM ops
             WHERE state = 'pending' AND next_attempt <= ?1
             ORDER BY id
             LIMIT ?2",
        )?;
        let rows = st.query_map(params![now, limit as i64], |r| Ok(op_from_row(r)))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Every operation still waiting, whatever its backoff — for `status`.
    pub fn queued_op_count(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM ops WHERE state = 'pending'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// Operations that were abandoned: §5.3's conflicts.
    pub fn failed_ops(&self) -> Result<Vec<Op>> {
        let mut st = self.conn.prepare(
            "SELECT id, kind, message_id, folder_path, target_path, payload, state,
                    attempts, last_error, created, next_attempt,
                    source_uid, source_uidvalidity
             FROM ops WHERE state = 'failed' ORDER BY id",
        )?;
        let rows = st.query_map([], |r| Ok(op_from_row(r)))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    pub fn complete_op(&mut self, id: i64) -> Result<()> {
        self.transaction("complete_op", |tx| {
            tx.execute("UPDATE ops SET state = 'done' WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// Records a failed attempt and schedules the retry, or gives up.
    ///
    /// `permanent` is for failures no retry can fix — the message is gone, the
    /// destination folder does not exist — where backing off would only delay
    /// the moment the user finds out.
    pub fn fail_op(&mut self, id: i64, error: &str, permanent: bool) -> Result<OpState> {
        let now = self.clock.now();
        let mut state = OpState::Pending;
        self.transaction("fail_op", |tx| {
            let attempts: u32 = tx
                .query_row("SELECT attempts FROM ops WHERE id = ?1", [id], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as u32 + 1)
                .unwrap_or(1);

            if permanent || attempts >= MAX_ATTEMPTS {
                state = OpState::Failed;
                tx.execute(
                    "UPDATE ops SET state = 'failed', attempts = ?2, last_error = ?3 WHERE id = ?1",
                    params![id, attempts as i64, error],
                )?;
            } else {
                tx.execute(
                    "UPDATE ops SET attempts = ?2, last_error = ?3, next_attempt = ?4 WHERE id = ?1",
                    params![id, attempts as i64, error, now + backoff_seconds(attempts)],
                )?;
            }
            Ok(())
        })?;
        Ok(state)
    }

    /// Forgets completed operations. Kept briefly rather than deleted on
    /// completion so `--trace` and a bug report can still see what was pushed.
    pub fn prune_ops(&mut self, older_than: Timestamp) -> Result<u64> {
        self.transaction("prune_ops", |tx| {
            let n = tx.execute(
                "DELETE FROM ops WHERE state = 'done' AND created < ?1",
                [older_than],
            )?;
            Ok(n as u64)
        })
    }

    /// Clears a conflict once the user has seen it.
    pub fn clear_failed_ops(&mut self) -> Result<u64> {
        self.transaction("clear_failed_ops", |tx| {
            let n = tx.execute("DELETE FROM ops WHERE state = 'failed'", [])?;
            Ok(n as u64)
        })
    }
}

/// Folds a new flag change into a pending one.
///
/// Later wins per flag: `+seen` then `-seen` is `-seen`, not both. Keeping both
/// would push a `STORE` whose result depends on which order the server applied
/// them in.
fn merge_flags(into: &mut FlagChange, new: &FlagChange) {
    for f in &new.add {
        into.remove.retain(|x| x != f);
        if !into.add.contains(f) {
            into.add.push(f.clone());
        }
    }
    for f in &new.remove {
        into.add.retain(|x| x != f);
        if !into.remove.contains(f) {
            into.remove.push(f.clone());
        }
    }
}

fn op_from_row(r: &rusqlite::Row<'_>) -> Op {
    Op {
        id: r.get(0).unwrap_or_default(),
        kind: r
            .get::<_, String>(1)
            .ok()
            .and_then(|k| OpKind::parse(&k))
            .unwrap_or(OpKind::Flag),
        message_id: r.get(2).unwrap_or_default(),
        folder_path: r.get(3).ok().flatten(),
        target_path: r.get(4).ok().flatten(),
        flags: r
            .get::<_, String>(5)
            .ok()
            .and_then(|p| serde_json::from_str(&p).ok())
            .unwrap_or_default(),
        state: match r.get::<_, String>(6).unwrap_or_default().as_str() {
            "done" => OpState::Done,
            "failed" => OpState::Failed,
            _ => OpState::Pending,
        },
        attempts: r.get::<_, i64>(7).unwrap_or(0) as u32,
        last_error: r.get(8).ok().flatten(),
        created: r.get(9).unwrap_or(0),
        next_attempt: r.get(10).unwrap_or(0),
        source_uid: r.get(11).ok().flatten(),
        source_uidvalidity: r.get(12).ok().flatten(),
    }
}

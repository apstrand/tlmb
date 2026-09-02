//! Replaying the operation queue (§5.3).
//!
//! Push before pull. A sync pass drains the queue first and only then fetches
//! what changed, for a reason that is easy to get backwards: if the pull ran
//! first, it would fetch the server's old flags, overwrite the user's local
//! edit, and *then* push that edit — leaving the store briefly showing the
//! opposite of what the user asked for, and an operation whose effect nobody
//! can see. Pushing first means the pull that follows confirms our own write.
//!
//! Every operation is idempotent on replay, which is what makes a crash between
//! "the server applied it" and "we recorded that it applied" cost nothing: the
//! next pass does it again, and doing it again changes nothing.

use crate::error::{Error, Result};
use crate::imap::Client;
use crate::store::ops::{Op, OpKind, OpState};
use crate::store::Store;
use crate::trace::event;

/// What one drain of the queue did.
#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    pub applied: u32,
    /// Operations that failed transiently and will be retried.
    pub deferred: u32,
    /// Operations abandoned — §5.3's conflicts, which the next pull resolves in
    /// the server's favour.
    pub conflicts: u32,
}

/// How many operations one pass will push.
///
/// Bounded so a queue built up over a week offline does not turn the first
/// reconnect into an hour-long write storm that starves the pull.
const MAX_PER_PASS: usize = 500;

/// Drains the queue, oldest first.
pub fn replay(client: &mut Client, store: &mut Store) -> Result<ReplayReport> {
    let now = store.clock().now();
    let pending = store.pending_ops(now, MAX_PER_PASS)?;
    if pending.is_empty() {
        return Ok(ReplayReport::default());
    }

    let tracer = store.tracer().clone();
    tracer.emit(
        event::REPLAY_BEGIN,
        serde_json::json!({ "queued": pending.len() }),
    );

    let mut report = ReplayReport::default();
    for op in pending {
        // A dropped connection ends the pass rather than burning through the
        // rest of the queue's retry budget on an error that is about the socket
        // and not about any of these operations.
        match apply(client, store, &op) {
            Ok(()) => {
                store.complete_op(op.id)?;
                report.applied += 1;
                tracer.emit(
                    event::REPLAY_OP,
                    serde_json::json!({ "id": op.id, "kind": op.kind.as_str(), "result": "applied" }),
                );
            }
            Err(e) if is_fatal_connection_error(&e) => {
                store.fail_op(op.id, &e.to_string(), false)?;
                report.deferred += 1;
                tracer.emit(
                    event::REPLAY_OP,
                    serde_json::json!({ "id": op.id, "result": "connection lost", "error": e.to_string() }),
                );
                break;
            }
            Err(e) => {
                let permanent = is_permanent(&e);
                let state = store.fail_op(op.id, &e.to_string(), permanent)?;
                match state {
                    OpState::Failed => report.conflicts += 1,
                    _ => report.deferred += 1,
                }
                tracer.emit(
                    event::REPLAY_OP,
                    serde_json::json!({
                        "id": op.id,
                        "kind": op.kind.as_str(),
                        "result": state.as_str(),
                        "error": e.to_string(),
                    }),
                );
            }
        }
    }

    tracer.emit(
        event::REPLAY_END,
        serde_json::json!({
            "applied": report.applied,
            "deferred": report.deferred,
            "conflicts": report.conflicts,
        }),
    );
    Ok(report)
}

fn apply(client: &mut Client, store: &mut Store, op: &Op) -> Result<()> {
    let folder = op
        .folder_path
        .clone()
        .ok_or_else(|| Error::protocol("a queued operation has no source folder"))?;

    client.select(&folder)?;
    let Some(uid) = source_uid(client, store, op, &folder)? else {
        return match op.kind {
            // The message is not in the source folder, and the operation was to
            // take it out of there. That is the state the operation wanted.
            // Replaying after a crash between "the server did it" and "we wrote
            // that down" lands exactly here, and the answer is "done".
            OpKind::Move | OpKind::Delete => Ok(()),
            _ => Err(Error::Conflict(format!(
                "{} is no longer in {folder}",
                crate::id::short(&op.message_id)
            ))),
        };
    };

    match op.kind {
        OpKind::Flag => client.store_flags(uid, &op.flags.add, &op.flags.remove),
        OpKind::Move => {
            let target = target_of(op)?;
            let new_uid = client.move_message(uid, &target)?;
            // With UIDPLUS the server named the copy; recording it saves the
            // next pass a rediscovery, and closes the pending placement the
            // local move opened.
            if let Some(new_uid) = new_uid {
                store.confirm_placement(&op.message_id, &target, new_uid)?;
            }
            Ok(())
        }
        OpKind::Copy => {
            let target = target_of(op)?;
            let new_uid = client.copy_message(uid, &target)?;
            if let Some(new_uid) = new_uid {
                store.confirm_placement(&op.message_id, &target, new_uid)?;
            }
            Ok(())
        }
        OpKind::Delete => client.delete_message(uid),
    }
}

fn target_of(op: &Op) -> Result<String> {
    op.target_path
        .clone()
        .ok_or_else(|| Error::protocol(format!("a queued {} has no destination", op.kind.as_str())))
}

/// Finds the UID the operation's message has in its source folder, right now.
///
/// Three sources, cheapest first:
///
/// 1. The local placement — right whenever the operation did not remove it,
///    which is every `flag`.
/// 2. The UID recorded when the operation was queued, but *only* if the
///    folder's `UIDVALIDITY` still matches the one it was recorded under. An
///    optimistic move deletes the source placement, so this is the normal path
///    for `move`.
/// 3. A `SEARCH` by `Message-ID`. Costs a round trip, and is what makes a queue
///    that outlived a `UIDVALIDITY` reset still mean the right message.
///
/// `None` means the message is genuinely not in that folder any more.
fn source_uid(client: &mut Client, store: &Store, op: &Op, folder: &str) -> Result<Option<u32>> {
    if let Some(uid) = store.locate(&op.message_id, folder)? {
        return Ok(Some(uid));
    }

    let current_uidvalidity = store.sync_state().folder(folder).uidvalidity;
    if let Some(uid) = op.source_uid {
        let recorded = op.source_uidvalidity.unwrap_or(current_uidvalidity);
        if recorded == current_uidvalidity {
            return Ok(Some(uid));
        }
    }

    // A message with no `Message-ID` cannot be searched for; saying so is
    // better than guessing at a UID.
    let Some(header_id) = store.header_message_id(&op.message_id)? else {
        return Ok(None);
    };
    client.find_by_message_id(&header_id)
}

/// Whether the connection is gone, as opposed to this operation being bad.
fn is_fatal_connection_error(e: &Error) -> bool {
    matches!(
        e,
        Error::Network(_) | Error::Io(_) | Error::Timeout(_) | Error::Auth(_)
    )
}

/// Whether retrying could ever help.
///
/// A `NO` from the server about a message or folder that is not there will be a
/// `NO` forever; backing off eight times before saying so only delays the
/// moment the user finds out.
fn is_permanent(e: &Error) -> bool {
    matches!(
        e,
        Error::NotFound { .. } | Error::Conflict(_) | Error::Rejected { .. }
    )
}

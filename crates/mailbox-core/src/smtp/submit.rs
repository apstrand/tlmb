//! Draining the outbox: SMTP submission, then the `Sent` copy (§5.5).
//!
//! The two halves are deliberately separable. A message is delivered once the
//! server says `250`; filing a copy in `Sent` is bookkeeping that can fail on
//! its own and be retried on its own, and it must never be able to walk a
//! delivered message back into the queue.

use crate::error::{Error, Result};
use crate::imap::{Client, Credential};
use crate::smtp::{SendOutcome, SmtpClient, SmtpConfig};
use crate::store::outbox::{OutboxEntry, OutboxState};
use crate::store::Store;
use crate::trace::{event, Tracer};

/// What one drain of the outbox did.
#[derive(Debug, Clone, Default)]
pub struct SubmitReport {
    pub attempted: u32,
    pub sent: u32,
    /// Refused temporarily, or lost in the double-send window. Will be retried.
    pub deferred: u32,
    /// Refused permanently, or out of attempts.
    pub failed: u32,
    /// `Sent` copies filed on the server this pass.
    pub uploaded: u32,
}

/// Sends everything the outbox has ready.
///
/// The SMTP connection is opened lazily: an outbox with nothing due should not
/// cost a TCP connection, a TLS handshake, and an `AUTH` on every sync.
pub fn flush(
    store: &mut Store,
    cfg: &SmtpConfig,
    user: &str,
    credential: &Credential,
) -> Result<SubmitReport> {
    let now = store.clock().now();
    let due = store.sendable_outbox(now)?;
    if due.is_empty() {
        return Ok(SubmitReport::default());
    }

    let tracer = store.tracer().clone();
    let mut report = SubmitReport::default();
    let mut client = SmtpClient::connect(cfg, tracer.clone())?;
    client.authenticate(user, credential)?;

    for entry in due {
        report.attempted += 1;
        match send_one(&mut client, store, &entry, &tracer) {
            Ok(SendOutcome::Accepted { response }) => {
                store.mark_outbox_sent(entry.id, &response)?;
                report.sent += 1;
            }
            Ok(outcome) => {
                // §16: `Uncertain` retries. It is grouped with the deferred
                // rather than given its own counter because, to the outbox, it
                // *is* deferred — the difference is only that trying again may
                // deliver twice, which the decision accepts.
                let permanent = !outcome.should_retry();
                let state = store.mark_outbox_attempt(entry.id, &outcome.message(), permanent)?;
                match state {
                    OutboxState::Failed => report.failed += 1,
                    _ => report.deferred += 1,
                }
            }
            Err(e) => {
                // The conversation broke. Nothing was delivered on this
                // attempt's account, so it is retryable, and the rest of the
                // batch is abandoned rather than run against a dead socket.
                store.mark_outbox_attempt(entry.id, &e.to_string(), false)?;
                report.deferred += 1;
                break;
            }
        }
    }

    client.quit();
    Ok(report)
}

fn send_one(
    client: &mut SmtpClient,
    store: &Store,
    entry: &OutboxEntry,
    tracer: &Tracer,
) -> Result<SendOutcome> {
    let body = store.blobs().get(&entry.blob_id)?;
    tracer.emit(
        event::OUTBOX_ATTEMPT,
        serde_json::json!({
            "id": entry.id,
            "attempts": entry.attempts,
            "recipients": entry.envelope_to.len(),
            "bytes": body.len(),
        }),
    );

    let outcome = client.send(&entry.envelope_from, &entry.envelope_to, &body)?;
    tracer.emit(
        event::OUTBOX_RESULT,
        serde_json::json!({
            "id": entry.id,
            "outcome": match &outcome {
                SendOutcome::Accepted { .. } => "accepted",
                SendOutcome::Rejected { .. } => "rejected",
                SendOutcome::Deferred { .. } => "deferred",
                SendOutcome::Uncertain { .. } => "uncertain",
            },
            "detail": outcome.message(),
        }),
    );
    Ok(outcome)
}

/// Files delivered messages in the account's `Sent` folder (§5.5).
///
/// Skipped where the server files sent mail itself: Gmail does this for
/// anything submitted through its own SMTP, and appending would leave the user
/// with two of everything in a folder they did not ask us to touch.
pub fn upload_sent(client: &mut Client, store: &mut Store) -> Result<u32> {
    let pending = store.outbox_awaiting_upload()?;
    if pending.is_empty() {
        return Ok(0);
    }

    if client.has("X-GM-EXT-1") {
        // Mark them done rather than leaving them to be reconsidered on every
        // pass forever.
        for entry in &pending {
            store.mark_outbox_uploaded(entry.id)?;
        }
        return Ok(0);
    }

    let Some(sent) = store
        .folders()?
        .into_iter()
        .find(|f| f.special_use.as_deref() == Some("sent"))
        .map(|f| f.path)
    else {
        // No Sent folder is not an error worth failing a send over — the mail
        // went out. It is worth not retrying forever, though.
        for entry in &pending {
            store.mark_outbox_uploaded(entry.id)?;
        }
        return Ok(0);
    };

    let tracer = store.tracer().clone();
    let mut uploaded = 0;
    for entry in pending {
        let body = store.blobs().get(&entry.blob_id)?;
        // `\Seen`, because you have read what you wrote.
        match client.append(&sent, &["\\Seen".to_string()], entry.sent_at, &body) {
            Ok(_) => {
                store.mark_outbox_uploaded(entry.id)?;
                uploaded += 1;
                tracer.emit(
                    event::OUTBOX_SENT_UPLOADED,
                    serde_json::json!({ "id": entry.id, "folder": sent }),
                );
            }
            // A refused APPEND — over quota, most likely — leaves the flag
            // unset so the next pass tries again. The message is still sent.
            Err(e) => {
                tracer.emit(
                    event::OUTBOX_SENT_UPLOADED,
                    serde_json::json!({ "id": entry.id, "error": e.to_string() }),
                );
            }
        }
    }
    Ok(uploaded)
}

/// Whether an outbox failure should stop the caller.
///
/// Sending is best-effort from a sync pass's point of view: a mail server that
/// is down must not stop the mailbox from syncing.
pub fn is_soft_failure(e: &Error) -> bool {
    matches!(
        e,
        Error::Network(_) | Error::Io(_) | Error::Timeout(_) | Error::Auth(_)
    )
}

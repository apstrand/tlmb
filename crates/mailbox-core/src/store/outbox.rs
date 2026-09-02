//! The durable outbox (§5.5).
//!
//! > Outgoing mail goes to a durable local **outbox** first, then to SMTP
//! > submission. Sends are retried with backoff, survive restarts, and are
//! > deduplicated by an idempotency key (§6.5).
//!
//! "Survive restarts" is the requirement that decides the design: the entry is
//! a row in `meta.db` and the bytes are in the blob store, so a composed
//! message never exists only inside a process that can die.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::error::{Error, Result};
use crate::store::Store;

/// Where an outbox entry is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    /// Composed, not yet released. §7's drafts.
    Draft,
    /// §6.5's approval queue: waiting for a human.
    PendingApproval,
    /// Ready to send, or waiting out the undo-send window or a backoff.
    Queued,
    Sent,
    /// Permanently refused, or out of attempts.
    Failed,
    Cancelled,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxState::Draft => "draft",
            OutboxState::PendingApproval => "pending_approval",
            OutboxState::Queued => "queued",
            OutboxState::Sent => "sent",
            OutboxState::Failed => "failed",
            OutboxState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> OutboxState {
        match s {
            "draft" => OutboxState::Draft,
            "pending_approval" => OutboxState::PendingApproval,
            "sent" => OutboxState::Sent,
            "failed" => OutboxState::Failed,
            "cancelled" => OutboxState::Cancelled,
            _ => OutboxState::Queued,
        }
    }
}

/// One message on its way out.
#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub id: i64,
    pub message_id: String,
    pub blob_id: String,
    pub envelope_from: String,
    pub envelope_to: Vec<String>,
    pub state: OutboxState,
    pub idempotency_key: Option<String>,
    pub result: Option<String>,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created: Timestamp,
    /// The undo-send deadline (§5.5): not eligible before this.
    pub send_after: Timestamp,
    pub next_attempt: Timestamp,
    pub sent_at: Option<Timestamp>,
    pub sent_uploaded: bool,
}

/// What to put in a new entry.
#[derive(Debug, Clone)]
pub struct NewOutboxEntry {
    pub message_id: String,
    pub blob_id: String,
    pub envelope_from: String,
    pub envelope_to: Vec<String>,
    pub state: OutboxState,
    pub idempotency_key: Option<String>,
    /// Unix seconds before which the message must not go out.
    pub send_after: Timestamp,
}

/// Backoff for a deferred send.
///
/// Slower than the operation queue's: a `4xx` from a mail server usually means
/// greylisting or a rate limit, and hammering it is what keeps you greylisted.
/// One minute, doubling, capped at an hour.
pub fn backoff_seconds(attempts: u32) -> i64 {
    const BASE: i64 = 60;
    const CAP: i64 = 3600;
    BASE.saturating_mul(1i64 << attempts.min(6)).min(CAP)
}

/// How many times a send is attempted before it is abandoned.
///
/// With the backoff above this spans several hours, which covers an ordinary
/// greylisting delay and a short outage without leaving mail stuck forever.
pub const MAX_ATTEMPTS: u32 = 10;

/// §6.5: idempotency keys expire after 7 days.
pub const IDEMPOTENCY_TTL: i64 = 7 * 86_400;

impl Store {
    /// Adds a message to the outbox.
    ///
    /// When `idempotency_key` names an entry that already exists and has not
    /// expired, nothing is enqueued and the existing entry comes back — which
    /// is §6.5's "a retry with the same key returns the original result instead
    /// of sending twice".
    pub fn enqueue_outbox(&mut self, new: &NewOutboxEntry) -> Result<(i64, bool)> {
        let now = self.clock.now();
        if let Some(key) = &new.idempotency_key {
            if let Some(existing) = self.outbox_by_key(key, now)? {
                return Ok((existing.id, false));
            }
        }

        let to = serde_json::to_string(&new.envelope_to)?;
        let id = self.transaction("enqueue_outbox", |tx| {
            tx.execute(
                "INSERT INTO outbox
                    (message_id, blob_id, envelope_from, envelope_to, state,
                     idempotency_key, created, send_after, next_attempt)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![
                    new.message_id,
                    new.blob_id,
                    new.envelope_from,
                    to,
                    new.state.as_str(),
                    new.idempotency_key,
                    now,
                    new.send_after,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        })?;
        Ok((id, true))
    }

    /// Looks up a live entry by idempotency key. Expired keys do not match, so
    /// the same key is reusable after the TTL.
    pub fn outbox_by_key(&self, key: &str, now: Timestamp) -> Result<Option<OutboxEntry>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox
             WHERE idempotency_key = ?1 AND created >= ?2"
        ))?;
        Ok(st
            .query_row(params![key, now - IDEMPOTENCY_TTL], |r| {
                Ok(entry_from_row(r))
            })
            .ok())
    }

    pub fn outbox_entry(&self, id: i64) -> Result<OutboxEntry> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {OUTBOX_COLS} FROM outbox WHERE id = ?1"))?;
        st.query_row([id], |r| Ok(entry_from_row(r)))
            .map_err(|_| Error::not_found("outbox entry", id.to_string()))
    }

    /// Everything in the outbox, newest first.
    pub fn outbox_entries(&self, state: Option<OutboxState>) -> Result<Vec<OutboxEntry>> {
        let (sql, params): (String, Vec<rusqlite::types::Value>) = match state {
            Some(s) => (
                format!("SELECT {OUTBOX_COLS} FROM outbox WHERE state = ?1 ORDER BY id DESC"),
                vec![rusqlite::types::Value::Text(s.as_str().to_string())],
            ),
            None => (
                format!("SELECT {OUTBOX_COLS} FROM outbox ORDER BY id DESC"),
                Vec::new(),
            ),
        };
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(params), |r| {
            Ok(entry_from_row(r))
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Entries ready to go: queued, past their undo window, past their backoff.
    pub fn sendable_outbox(&self, now: Timestamp) -> Result<Vec<OutboxEntry>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox
             WHERE state = 'queued' AND send_after <= ?1 AND next_attempt <= ?1
             ORDER BY id"
        ))?;
        let rows = st.query_map([now], |r| Ok(entry_from_row(r)))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Sent messages whose `Sent` copy has not been filed yet (§5.5).
    pub fn outbox_awaiting_upload(&self) -> Result<Vec<OutboxEntry>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox
             WHERE state = 'sent' AND sent_uploaded = 0 ORDER BY id"
        ))?;
        let rows = st.query_map([], |r| Ok(entry_from_row(r)))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Records a delivery.
    ///
    /// Deliberately does not touch `sent_uploaded`: filing the `Sent` copy is a
    /// separate step that must not be able to un-send the message if it fails.
    pub fn mark_outbox_sent(&mut self, id: i64, response: &str) -> Result<()> {
        let now = self.clock.now();
        self.transaction("mark_outbox_sent", |tx| {
            tx.execute(
                "UPDATE outbox SET state = 'sent', sent_at = ?2, last_error = NULL,
                        result = COALESCE(result, ?3)
                 WHERE id = ?1",
                params![id, now, response],
            )?;
            Ok(())
        })
    }

    pub fn mark_outbox_uploaded(&mut self, id: i64) -> Result<()> {
        self.transaction("mark_outbox_uploaded", |tx| {
            tx.execute("UPDATE outbox SET sent_uploaded = 1 WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// Records a failed attempt: backs off, or gives up.
    pub fn mark_outbox_attempt(
        &mut self,
        id: i64,
        error: &str,
        permanent: bool,
    ) -> Result<OutboxState> {
        let now = self.clock.now();
        let mut state = OutboxState::Queued;
        self.transaction("mark_outbox_attempt", |tx| {
            let attempts: u32 = tx
                .query_row("SELECT attempts FROM outbox WHERE id = ?1", [id], |r| {
                    r.get::<_, i64>(0)
                })
                .map(|n| n as u32 + 1)
                .unwrap_or(1);

            if permanent || attempts >= MAX_ATTEMPTS {
                state = OutboxState::Failed;
                tx.execute(
                    "UPDATE outbox SET state = 'failed', attempts = ?2, last_error = ?3
                     WHERE id = ?1",
                    params![id, attempts as i64, error],
                )?;
            } else {
                tx.execute(
                    "UPDATE outbox SET attempts = ?2, last_error = ?3, next_attempt = ?4
                     WHERE id = ?1",
                    params![id, attempts as i64, error, now + backoff_seconds(attempts)],
                )?;
            }
            Ok(())
        })?;
        Ok(state)
    }

    /// Moves an entry to a new state — approval, cancellation, a manual retry.
    pub fn set_outbox_state(&mut self, id: i64, state: OutboxState) -> Result<()> {
        let now = self.clock.now();
        self.transaction("set_outbox_state", |tx| {
            let changed = tx.execute(
                // Sent is terminal: nothing may walk a delivered message back
                // into the queue, because that would send it again.
                "UPDATE outbox SET state = ?2, next_attempt = ?3, send_after = MIN(send_after, ?3)
                 WHERE id = ?1 AND state <> 'sent'",
                params![id, state.as_str(), now],
            )?;
            if changed == 0 {
                return Err(Error::usage(
                    "that message has already been sent; there is nothing to change",
                ));
            }
            Ok(())
        })
    }

    /// Records the reply the original call returned, for §6.5's key replay.
    pub fn set_outbox_result(&mut self, id: i64, result: &str) -> Result<()> {
        self.transaction("set_outbox_result", |tx| {
            tx.execute(
                "UPDATE outbox SET result = ?2 WHERE id = ?1",
                params![id, result],
            )?;
            Ok(())
        })
    }

    pub fn outbox_pending_count(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM outbox WHERE state IN ('queued', 'pending_approval')",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// How many messages were accepted in the last hour, for the policy's rate
    /// limit (§6.5).
    pub fn sends_since(&self, since: Timestamp) -> Result<u32> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM outbox WHERE state = 'sent' AND sent_at >= ?1",
            [since],
            |r| r.get::<_, i64>(0),
        )? as u32)
    }
}

const OUTBOX_COLS: &str = "id, message_id, blob_id, envelope_from, envelope_to, state, \
     idempotency_key, result, attempts, last_error, created, send_after, next_attempt, \
     sent_at, sent_uploaded";

fn entry_from_row(r: &rusqlite::Row<'_>) -> OutboxEntry {
    OutboxEntry {
        id: r.get(0).unwrap_or_default(),
        message_id: r.get(1).unwrap_or_default(),
        blob_id: r.get(2).unwrap_or_default(),
        envelope_from: r.get(3).unwrap_or_default(),
        envelope_to: r
            .get::<_, String>(4)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        state: OutboxState::parse(&r.get::<_, String>(5).unwrap_or_default()),
        idempotency_key: r.get(6).ok().flatten(),
        result: r.get(7).ok().flatten(),
        attempts: r.get::<_, i64>(8).unwrap_or(0) as u32,
        last_error: r.get(9).ok().flatten(),
        created: r.get(10).unwrap_or(0),
        send_after: r.get(11).unwrap_or(0),
        next_attempt: r.get(12).unwrap_or(0),
        sent_at: r.get(13).ok().flatten(),
        sent_uploaded: r.get::<_, i64>(14).unwrap_or(0) != 0,
    }
}

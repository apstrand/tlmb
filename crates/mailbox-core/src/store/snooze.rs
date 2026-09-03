//! Snooze: set a message aside until a chosen instant (§7).
//!
//! Deliberately the smallest thing that works, because §7 says so:
//!
//! > **Snooze** is a low-priority feature and starts local-only: a timer plus
//! > a local state record, which means a snoozed message reappears on the
//! > device that snoozed it, whenever that device is next running.
//!
//! Two consequences are visible in the code below. Nothing is queued for the
//! server — a snooze is not an IMAP concept, and inventing one (a `\Snoozed`
//! keyword, a hidden folder) would put a private decision into a shared
//! mailbox and surprise every other client the user runs. And there is no
//! timer thread: a message is asleep exactly while `until` is in the future,
//! so waking is a comparison at read time rather than an event someone has to
//! be running to receive. A laptop that was shut when the snooze expired shows
//! the message the moment it is opened, which is what the user meant.
//!
//! The device name is recorded so every surface can say *which* machine holds
//! the timer. That is the honest limitation §7 asks to be stated rather than
//! papered over; it becomes a real cross-device snooze with the §5.4 state
//! folder in M8.

use rusqlite::params;

use crate::error::{Error, Result};
use crate::store::Store;

/// One sleeping message, as `snooze --list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snooze {
    pub message_id: String,
    pub subject: String,
    /// Unix seconds at which it returns.
    pub until: i64,
    /// The machine that will wake it.
    pub device: String,
}

impl Store {
    /// Sets a message aside until `until` (unix seconds).
    ///
    /// Re-snoozing an already-snoozed message replaces the deadline, which is
    /// what "snooze this again for a week" has to mean.
    pub fn snooze(&mut self, message_id: &str, until: i64, device: &str, now: i64) -> Result<()> {
        self.transaction("snooze", |tx| {
            let rowid: i64 = tx
                .query_row(
                    "SELECT rowid FROM messages WHERE id = ?1",
                    [message_id],
                    |r| r.get(0),
                )
                .map_err(|_| Error::not_found("message", message_id))?;

            tx.execute(
                "INSERT INTO snoozed (message_rowid, until, device, created)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(message_rowid) DO UPDATE SET
                     until = excluded.until,
                     device = excluded.device,
                     created = excluded.created",
                params![rowid, until, device, now],
            )?;
            Ok(())
        })
    }

    /// Wakes a message now. Returns false if it was not asleep.
    pub fn unsnooze(&mut self, message_id: &str) -> Result<bool> {
        self.transaction("unsnooze", |tx| {
            let rowid: i64 = tx
                .query_row(
                    "SELECT rowid FROM messages WHERE id = ?1",
                    [message_id],
                    |r| r.get(0),
                )
                .map_err(|_| Error::not_found("message", message_id))?;
            let n = tx.execute("DELETE FROM snoozed WHERE message_rowid = ?1", [rowid])?;
            Ok(n > 0)
        })
    }

    /// Every message still asleep at `now`, soonest first.
    ///
    /// Expired rows are not returned and not deleted. Leaving them costs a row
    /// each and buys an important property: waking needs no writer. A store
    /// opened read-only — which is most of them, most of the time (§10) —
    /// still shows woken mail in the right place.
    pub fn snoozed(&self, now: i64) -> Result<Vec<Snooze>> {
        let mut st = self.conn.prepare(
            "SELECT m.id, m.subject, s.until, s.device
               FROM snoozed s JOIN messages m ON m.rowid = s.message_rowid
              WHERE s.until > ?1
              ORDER BY s.until, m.id",
        )?;
        let rows = st.query_map([now], |r| {
            Ok(Snooze {
                message_id: r.get(0)?,
                subject: r.get(1)?,
                until: r.get(2)?,
                device: r.get(3)?,
            })
        })?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// The snooze on one message, if it is still asleep.
    pub fn snooze_of(&self, message_id: &str, now: i64) -> Result<Option<Snooze>> {
        let mut st = self.conn.prepare(
            "SELECT m.id, m.subject, s.until, s.device
               FROM snoozed s JOIN messages m ON m.rowid = s.message_rowid
              WHERE m.id = ?1 AND s.until > ?2",
        )?;
        let mut rows = st.query_map(params![message_id, now], |r| {
            Ok(Snooze {
                message_id: r.get(0)?,
                subject: r.get(1)?,
                until: r.get(2)?,
                device: r.get(3)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Drops rows whose deadline has passed. Housekeeping, never correctness:
    /// [`Store::snoozed`] already ignores them.
    pub fn prune_snoozes(&mut self, now: i64) -> Result<u64> {
        self.transaction("prune_snoozes", |tx| {
            Ok(tx.execute("DELETE FROM snoozed WHERE until <= ?1", [now])? as u64)
        })
    }
}

/// Parses the "until" of a snooze against the injected clock.
///
/// The query grammar's date forms (§6.2), plus one extension: a bare duration
/// like `2h` is read as "from now". `before:` and `after:` require the sign
/// because `7d` there could mean either direction, but a snooze only ever
/// points forwards, so demanding a `+` would be pedantry.
///
/// A time already past is refused rather than accepted as a no-op. "Snooze
/// until yesterday" is a typo every time — most often a `-` where a `+` was
/// meant — and silently doing nothing is the least helpful reading of it.
pub fn parse_until(input: &str, clock: &crate::Clock) -> Result<i64> {
    let s = input.trim();
    if s.is_empty() {
        return Err(Error::usage(
            "snooze until when? try `2h`, `tomorrow`, or a date",
        ));
    }

    // The grammar's own forms first, so a date is read as a date. Only if that
    // fails is a bare `2h` retried as `+2h` — a fallback rather than a guess,
    // which is what keeps `2026-09-10` from being read as a duration because
    // it happens to start with a digit.
    let until = crate::query::parse::parse_date(s, clock)
        .or_else(|| crate::query::parse::parse_date(&format!("+{s}"), clock))
        .ok_or_else(|| {
            Error::usage(format!(
                "`{input}` is not a time; try `2h`, `tomorrow`, `+3d`, or `2026-09-10`"
            ))
        })?;

    let now = clock.now();
    if until <= now {
        return Err(Error::usage(format!(
            "`{input}` is in the past ({} seconds ago); a snooze has to point forwards",
            now - until
        )));
    }
    Ok(until)
}

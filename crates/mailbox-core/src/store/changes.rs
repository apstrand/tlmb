//! The change feed behind `mailbox watch` (§6.4).
//!
//! Every mutation that a caller outside this process could care about appends a
//! row here in the same transaction that makes the change. Same transaction is
//! the whole design: an event that could commit without its change would make
//! an agent act on mail that does not exist, and a change that could commit
//! without its event would make one that polls `--cursor` miss it forever.
//!
//! The feed is a log, not a queue — reading does not consume. Several watchers
//! can follow it at their own pace, and one that dies resumes from the last
//! `seq` it finished acting on.

use rusqlite::{params, Transaction};

use crate::error::Result;
use crate::store::Store;

/// What happened. Serialized as the `event` field of §6.4's stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A message this store had never seen, in whatever folder brought it in.
    New,
    /// Flags changed — locally, or on the server and pulled in by a sync.
    Flag,
    Move,
    Copy,
    Tag,
    /// An outbox entry left for SMTP and was accepted (§5.5).
    Sent,
    /// Gone from the store: its last placement disappeared.
    Deleted,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::New => "new",
            ChangeKind::Flag => "flag",
            ChangeKind::Move => "move",
            ChangeKind::Copy => "copy",
            ChangeKind::Tag => "tag",
            ChangeKind::Sent => "sent",
            ChangeKind::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<ChangeKind> {
        Some(match s {
            "new" => ChangeKind::New,
            "flag" => ChangeKind::Flag,
            "move" => ChangeKind::Move,
            "copy" => ChangeKind::Copy,
            "tag" => ChangeKind::Tag,
            "sent" => ChangeKind::Sent,
            "deleted" => ChangeKind::Deleted,
            _ => return None,
        })
    }

    /// Every kind, for `--events` validation and for `--help`.
    pub const ALL: &'static [ChangeKind] = &[
        ChangeKind::New,
        ChangeKind::Flag,
        ChangeKind::Move,
        ChangeKind::Copy,
        ChangeKind::Tag,
        ChangeKind::Sent,
        ChangeKind::Deleted,
    ];
}

pub use mailbox_api::Change;

/// One event, as the site that makes the change describes it.
///
/// A struct rather than seven positional arguments: the call sites are inside
/// transactions doing something else, and `record(tx, now, kind, Some(id),
/// None, true, &json)` is exactly the shape that ends up with two of them
/// swapped.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    pub kind: ChangeKind,
    pub message: Option<&'a str>,
    /// Where it arrived, or where it went.
    pub folder: Option<&'a str>,
    /// True when this device made the change; false when a sync brought it in.
    pub local: bool,
    pub detail: serde_json::Value,
}

impl<'a> Event<'a> {
    pub fn new(kind: ChangeKind, message: &'a str) -> Event<'a> {
        Event {
            kind,
            message: Some(message),
            folder: None,
            local: false,
            detail: serde_json::Value::Null,
        }
    }

    pub fn local(mut self) -> Event<'a> {
        self.local = true;
        self
    }

    pub fn in_folder(mut self, folder: impl Into<Option<&'a str>>) -> Event<'a> {
        self.folder = folder.into();
        self
    }

    pub fn with(mut self, detail: serde_json::Value) -> Event<'a> {
        self.detail = detail;
        self
    }
}

/// Appends an event inside a caller's transaction.
///
/// Takes a `Transaction` rather than `&mut Store` precisely so it cannot be
/// called outside the one that makes the change.
pub fn record(tx: &Transaction<'_>, now: i64, ev: &Event<'_>) -> Result<()> {
    tx.execute(
        "INSERT INTO changes (at, kind, message_id, folder, local, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            now,
            ev.kind.as_str(),
            ev.message,
            ev.folder,
            ev.local as i64,
            if ev.detail.is_null() {
                "{}".to_string()
            } else {
                serde_json::to_string(&ev.detail)?
            },
        ],
    )?;
    // Trimming on every append would be a delete scan per flag set. Once every
    // few hundred events keeps the log within a rounding error of `RETAIN`.
    if tx.last_insert_rowid() % 512 == 0 {
        trim(tx)?;
    }
    Ok(())
}

/// How many events to keep. A watcher that has been away longer than this has
/// missed events, and is told so rather than silently resumed (see
/// [`Store::changes_since`]).
///
/// The bound is on rows rather than age because the cost this protects against
/// is store size, and a mailbox that receives nothing should not shrink its own
/// history. At roughly 100 bytes a row this is a few megabytes at worst.
pub const RETAIN: i64 = 50_000;

/// Trims the feed. Called after appending, cheaply and not every time.
pub fn trim(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "DELETE FROM changes WHERE seq <= (SELECT MAX(seq) - ?1 FROM changes)",
        [RETAIN],
    )?;
    Ok(())
}

/// A page of the feed, and whether the cursor that asked for it was still live.
#[derive(Debug, Clone)]
pub struct ChangePage {
    pub changes: Vec<Change>,
    /// True when the requested cursor was older than the oldest retained event.
    ///
    /// The caller has missed events. Saying so is the difference between an
    /// agent that resyncs and one that quietly acts on a partial mailbox.
    pub truncated: bool,
    /// The seq to pass back, even when nothing was returned.
    pub cursor: i64,
}

impl Store {
    /// Reads events after `after`, oldest first.
    pub fn changes_since(&self, after: i64, limit: usize) -> Result<ChangePage> {
        let oldest: Option<i64> = self
            .conn
            .query_row("SELECT MIN(seq) FROM changes", [], |r| r.get(0))?;
        let latest: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| {
                    r.get(0)
                })?;

        // `after + 1` is the first event the caller has not seen. If the log no
        // longer starts at or before it, something was trimmed between its
        // reads.
        let truncated = match oldest {
            Some(min) => after > 0 && min > after + 1,
            None => false,
        };

        let mut st = self.conn.prepare(
            "SELECT seq, at, kind, message_id, folder, local, payload
             FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let account = self.account.clone();
        let changes: Vec<Change> = st
            .query_map(params![after, limit as i64], |r| {
                Ok(Change {
                    seq: r.get(0)?,
                    at: r.get(1)?,
                    event: r.get(2)?,
                    account: account.clone(),
                    message: r.get(3)?,
                    folder: r.get(4)?,
                    local: r.get::<_, i64>(5)? != 0,
                    detail: r
                        .get::<_, String>(6)
                        .ok()
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null),
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        let cursor = changes.last().map(|c| c.seq).unwrap_or(after.max(0));
        Ok(ChangePage {
            changes,
            truncated,
            // A caller starting from nothing wants "from here on", not a
            // replay of the whole log; `--since` is how you ask for history.
            cursor: if after < 0 { latest } else { cursor },
        })
    }

    /// The newest seq, which is where a watcher with no cursor starts.
    pub fn latest_change(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |r| {
                r.get(0)
            })?)
    }

    /// Events at or after a wall-clock time, for `watch --since`.
    pub fn change_seq_at(&self, at: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MIN(seq), (SELECT COALESCE(MAX(seq), 0) + 1 FROM changes)) - 1
             FROM changes WHERE at >= ?1",
            [at],
            |r| r.get(0),
        )?)
    }
}

//! The one request/response contract (§4). The CLI's `--json` output *is* this
//! contract, so anything the GUI can do a script can do — and the test suite is
//! its largest consumer (§14.2).
//!
//! Two rules govern changes here:
//!
//! 1. Adding an optional field is fine. Removing one, renaming one, or changing
//!    its type requires a `mailbox/v2` [`SCHEMA_VERSION`] and a deprecation
//!    period (§14.4).
//! 2. Field declaration order is the emitted order. Sorting a struct's fields
//!    "for tidiness" changes every golden file downstream.

pub mod error;
pub mod types;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use error::{ErrorBody, ErrorCode, ErrorResponse};
pub use types::*;

/// Stamped on every successful `--json` document.
pub const SCHEMA_VERSION: &str = "mailbox/v1";

/// The envelope every command's `--json` output carries.
///
/// `payload` is flattened, so `ls` emits
/// `{"schema":"mailbox/v1","command":"ls","messages":[…],"cursor":null}`
/// rather than burying the rows under a `result` key.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Reply<T> {
    pub schema: String,
    pub command: String,
    #[serde(flatten)]
    pub payload: T,
}

impl<T> Reply<T> {
    pub fn new(command: &str, payload: T) -> Self {
        Reply {
            schema: SCHEMA_VERSION.to_string(),
            command: command.to_string(),
            payload,
        }
    }
}

/// A paginated list. Cursors, not offsets (§6.4) — an offset silently skips or
/// repeats rows when the underlying set changes between calls.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque; pass back as `--cursor` for the next page. `null` at the end.
    pub cursor: Option<String>,
    /// Total matching rows, when counting them was cheap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, cursor: Option<String>) -> Self {
        Page {
            items,
            cursor,
            total: None,
        }
    }
}

/// `ls` and `search` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageList {
    pub messages: Vec<MessageSummary>,
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// True when the store is still filling in older mail, so this listing is
    /// drawn from part of the mailbox (§5.1).
    ///
    /// A bare boolean rather than the whole of [`Coverage`], because it rides
    /// on every listing and every search: one field to check on the result you
    /// are about to act on, with `mailbox status` for the detail. Always
    /// present — a flag that appears only when the news is bad is one nobody
    /// writes the check for.
    #[serde(default)]
    pub partial: bool,
}

/// `ls --threads` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ThreadList {
    pub threads: Vec<Thread>,
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// `folders` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FolderList {
    pub folders: Vec<Folder>,
}

/// `accounts` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountList {
    pub accounts: Vec<Account>,
}

/// `show` payload. One document whether the caller asked for a message or a
/// whole thread; `thread` is absent for a single message.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShowResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<MessageDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
}

/// `outbox` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutboxList {
    pub messages: Vec<OutboxItem>,
}

/// `snooze` payload (§7).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SnoozeList {
    pub snoozed: Vec<Snoozed>,
}

/// `flag`, `mv`, `cp`, `rm`, `archive`, `spam` payload: what a write did.
///
/// One type for all six because they are one operation with six names (§6.1),
/// and because a caller that handles `flag` should not need new code to handle
/// `archive`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteResult {
    /// How many messages actually changed. Zero is a success: the flag was
    /// already set, or the message was already in that folder.
    pub changed: usize,
    pub messages: Vec<WriteRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WriteRecord {
    pub id: String,
    /// The folder it is in after the write.
    pub folder: String,
    /// The flags after the write, not the ones asked for.
    pub flags: Vec<String>,
    pub changed: bool,
}

/// `attach` payload: what a message carries, or what was written out.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentList {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Present for `attach --extract`: the files written, and where.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extracted: Vec<Extracted>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Extracted {
    pub part: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub mime_type: String,
    pub bytes: u64,
    /// Where it was written. §9: never opened, only saved.
    pub path: String,
}

/// `notify` payload (§11): what the platform offers, and what is waiting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct NotifyStatus {
    pub notifier: String,
    /// False when no desktop notifier was found. Not an error: §11 makes
    /// notifications an enhancement, and agent mode raises none at all.
    pub available: bool,
    /// `--test` only: whether the test notification went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_sync: Option<usize>,
    /// What would be announced by the next sync, without announcing it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<MessageSummary>,
}

/// `watch` payload: one event of the change feed (§6.4).
///
/// Emitted one per line under `--ndjson`, which is what makes `watch` a stream
/// a caller can act on as it arrives rather than a document that never ends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Change {
    /// The cursor: pass it back as `--cursor` to resume after this event.
    pub seq: i64,
    /// Unix seconds.
    pub at: i64,
    /// `new`, `flag`, `move`, `copy`, `tag`, `sent`, `deleted`.
    pub event: String,
    pub account: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// True when this device made the change, false when a sync brought it in.
    ///
    /// An agent that flags what it reads would otherwise wake itself up.
    pub local: bool,
    /// Kind-specific detail: `flags` after a `flag`, `from` for a `move`.
    ///
    /// Always present, `{}` when there is nothing to add: a consumer matching
    /// on `detail.flags` should not also have to handle a missing `detail`.
    pub detail: serde_json::Value,
}

/// `watch` as a single document, for a caller that asked for a bounded slice
/// of the feed rather than a stream.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChangeList {
    pub changes: Vec<Change>,
    /// Pass back as `--cursor`. Present even when no events were returned, so
    /// a caller that polls does not re-read what it has seen.
    ///
    /// Opaque: a workspace is several stores, and the position in one of them
    /// is not a position in the others.
    pub cursor: String,
    /// True when the feed had been trimmed past the requested cursor: events
    /// were missed, and a full listing is the way back to a known state.
    ///
    /// Always present. This is the field a caller must check to know its view
    /// is complete, and one that appears only when the news is bad is one
    /// nobody writes the check for.
    pub truncated: bool,
}

/// One message set aside, and where its timer lives.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Snoozed {
    pub id: String,
    pub account: String,
    pub subject: String,
    /// RFC3339 UTC. When it returns to the inbox.
    pub until: String,
    /// The machine holding the timer.
    ///
    /// Reported because a snooze is local until the §5.4 state folder lands in
    /// M8: the message reappears on *this* device, whenever it is next
    /// running, and a client that did not say so would be implying a sync that
    /// is not happening.
    pub device: String,
}

/// `sync` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SyncResult {
    pub synced: Vec<SyncReport>,
    /// What the outbox did during the pass (§5.5). Absent when it had nothing
    /// to do, which is the common case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted: Option<SubmitReport>,
}

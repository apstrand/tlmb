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

/// `sync` payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SyncResult {
    pub synced: Vec<SyncReport>,
    /// What the outbox did during the pass (§5.5). Absent when it had nothing
    /// to do, which is the common case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted: Option<SubmitReport>,
}

//! The nouns of the contract. Field order here *is* the wire order (§6.4):
//! reordering a struct's fields is a visible change, so don't.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A mailbox address, as parsed from a header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Address {
    /// The display name, if the header carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `local@domain`, lowercased in the domain only.
    pub address: String,
}

impl Address {
    pub fn new(name: Option<String>, address: impl Into<String>) -> Self {
        Address {
            name,
            address: address.into(),
        }
    }
}

/// One row of `ls`, `search`, and the thread listings.
///
/// Everything here is answerable from `meta.db` alone — no blob read, no
/// network — which is what keeps `ls` inside its 100 ms budget (§10).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageSummary {
    /// The stable id of §5.2, abbreviated to the shortest unambiguous prefix
    /// unless `--full-ids` was given.
    pub id: String,
    /// Account id this message belongs to.
    pub account: String,
    /// Server folder path, as the server spells it.
    pub folder: String,
    /// Thread this message groups into (§5.6). The id of the thread's root.
    pub thread_id: String,
    /// RFC3339 UTC. The `Date` header, falling back to `INTERNALDATE`.
    pub date: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub subject: String,
    /// System flags and keywords, lowercased and without the leading `\`.
    pub flags: Vec<String>,
    /// Local tags (§5.4). Empty until M8 makes them portable.
    pub tags: Vec<String>,
    pub has_attachment: bool,
    /// RFC822.SIZE, in bytes.
    pub size: u64,
    /// First line or so of the text body, when one is cached locally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// How `show` rendered the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    /// `text/plain` as it arrived, or HTML flattened to text.
    Text,
    /// HTML converted to Markdown for LLM consumers (§6.4).
    Md,
    /// Sanitized HTML.
    Html,
    /// Exact RFC5322 bytes, lossy-decoded for JSON transport.
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Body {
    pub format: BodyFormat,
    pub content: String,
    /// True when `--max-body-bytes` cut the content short (§6.4).
    pub truncated: bool,
}

/// One MIME part worth naming: an attachment, or an inline image.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
    /// The MIME part path (`2.1`), which is what `attach <id> <part>` takes.
    pub part: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub mime_type: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// False for `Content-Disposition: inline` parts.
    pub is_attachment: bool,
}

/// The full form of a message: `show <id> --json`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageDetail {
    #[serde(flatten)]
    pub summary: MessageSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub reply_to: Vec<Address>,
    pub bcc: Vec<Address>,
    /// Headers not already broken out above, in the order they appeared.
    pub headers: Vec<Header>,
    /// Absent when the body has not been fetched yet and we are offline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// A conversation (§5.6), as `show --thread` and `ls --threads` report it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub id: String,
    pub subject: String,
    pub message_count: u32,
    pub unread_count: u32,
    /// RFC3339 of the newest message in the thread.
    pub last_date: String,
    /// Every distinct participant, most recent first.
    pub participants: Vec<Address>,
    pub messages: Vec<MessageSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Folder {
    pub account: String,
    /// The path as the server spells it, with its own delimiter.
    pub path: String,
    /// The last path segment, decoded from modified UTF-7.
    pub name: String,
    /// `inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, `all` — from
    /// `SPECIAL-USE` where advertised, else guessed from the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub special_use: Option<String>,
    pub total: u32,
    pub unread: u32,
    /// False for `\Noselect` container folders.
    pub selectable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Account {
    pub id: String,
    pub address: String,
    /// The name sent to the server, which is the address unless `imap_user`
    /// overrides it. Reported separately because "wrong username" and "wrong
    /// password" fail identically from the outside, and this is the one that
    /// can be checked without a trace.
    pub login_user: String,
    pub imap_host: String,
    pub imap_port: u16,
    /// Whether a credential for this account is currently resolvable.
    pub has_credential: bool,
    /// RFC3339 of the last completed sync pass, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
}

/// What one `sync --once` pass did — the shape every end-to-end test asserts on.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SyncReport {
    pub account: String,
    pub folders_examined: u32,
    pub messages_added: u32,
    pub messages_updated: u32,
    pub messages_removed: u32,
    pub bodies_fetched: u32,
    /// Queued local writes pushed to the server this pass (§5.3).
    pub ops_applied: u32,
    /// Queued writes that failed transiently and will be retried.
    pub ops_deferred: u32,
    /// Queued writes abandoned. The next pull resolves them in the server's
    /// favour, and `mailbox status` carries the note.
    pub ops_conflicted: u32,
    /// Folders whose `UIDVALIDITY` changed, forcing a rescan.
    pub uidvalidity_resets: Vec<String>,
    pub duration_ms: u64,
}

/// What `send`, `compose`, and `reply` report.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SendResult {
    /// The outbox entry. Stable for the life of the entry, and what
    /// `outbox approve|retry|cancel` takes.
    pub id: i64,
    /// The stable id of §5.2, so the sent message can be found once it is
    /// filed in `Sent`.
    pub message_id: String,
    /// `queued` · `pending_approval` (§6.5) · `sent` · `dry_run`.
    pub status: String,
    /// The SMTP envelope — every address the message is actually delivered to,
    /// including `Bcc`, which appears in no header.
    pub recipients: Vec<String>,
    pub bytes: u64,
    /// RFC3339 of the end of the undo-send window (§5.5), when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_after: Option<String>,
    /// True when an idempotency key matched an existing entry, so this call
    /// sent nothing (§6.5).
    pub duplicate: bool,
    /// `--dry-run` only: the exact bytes that would have been transmitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
}

/// One message in the outbox.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutboxItem {
    pub id: i64,
    pub message_id: String,
    pub subject: String,
    pub from: String,
    pub recipients: Vec<String>,
    /// `draft` · `pending_approval` · `queued` · `sent` · `failed` · `cancelled`.
    pub state: String,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<String>,
    /// Whether the `Sent` copy has been filed on the server (§5.5).
    pub filed: bool,
}

/// What one outbox flush did.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SubmitReport {
    pub attempted: u32,
    pub sent: u32,
    /// Refused temporarily, or lost in the double-send window (§16). Retried.
    pub deferred: u32,
    pub failed: u32,
    /// `Sent` copies filed this pass.
    pub uploaded: u32,
}

/// `mailbox status`: what the store holds and what is wrong with it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Status {
    pub accounts: Vec<AccountStatus>,
    /// Total size of `meta.db` plus `blobs/`, in bytes.
    pub store_bytes: u64,
}

/// A local write that could not be pushed, reported by `mailbox status` (§5.3).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Conflict {
    pub message: String,
    /// `flag`, `move`, `copy`, `delete`.
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    pub reason: String,
    /// RFC3339 of when the write was made locally.
    pub queued_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AccountStatus {
    pub account: String,
    pub address: String,
    pub messages: u64,
    pub unread: u64,
    pub folders: u32,
    /// True when this process holds the account's owner lock (§4.1).
    pub owner: bool,
    /// Local writes not yet pushed (§5.3).
    pub queued_ops: u64,
    /// Messages waiting in the outbox, including any waiting for approval.
    pub outbox_pending: u64,
    /// Writes that were abandoned and lost to the server's version.
    pub conflicts: Vec<Conflict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

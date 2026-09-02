//! The stable message id of §5.2.
//!
//! The id must survive a folder move, a `UIDVALIDITY` reset, a re-download, and
//! must be identical on every device — so it is derived from the message, never
//! from anything the server assigns. That rules out UID, which changes, and
//! rules out a local rowid, which differs per device.

use crate::hex;

/// How many hex characters the CLI shows by default. Long enough that a
/// collision inside one account's listing is not a practical concern, short
/// enough to paste.
pub const SHORT_LEN: usize = 7;

/// The 32-byte digest, hex-encoded. Kept as a `String` because it crosses the
/// JSON contract far more often than it is compared.
pub type MessageId = String;

/// The fields used when a message has no usable `Message-ID`.
///
/// Weaker than a `Message-ID`, and deliberately so: it is stable for the same
/// bytes but two genuinely distinct messages with the same date, sender,
/// subject, and size will collide. That is the correct trade — those are almost
/// always duplicates of each other anyway, and the alternative (falling back to
/// UID) breaks the "same on every device" guarantee that the whole scheme
/// exists for.
pub struct Fallback<'a> {
    /// `INTERNALDATE` or `Date`, as unix seconds.
    pub date: i64,
    /// The first `From` address, lowercased.
    pub from: &'a str,
    /// The raw `Subject`, undecoded.
    pub subject: &'a str,
    /// `RFC822.SIZE`.
    pub size: u64,
}

/// Derives the id for a message in `account`.
///
/// Passing `Some(msgid)` that turns out to be unusable falls through to
/// `fallback`, so callers can hand over whatever the header parser produced
/// without pre-screening it.
pub fn message_id(account: &str, msgid: Option<&str>, fallback: &Fallback<'_>) -> MessageId {
    let mut h = blake3::Hasher::new();
    // A domain tag, so a digest computed here can never be confused with one
    // computed for a thread or a blob.
    h.update(b"mailbox/msgid/v1\x1f");
    h.update(account.as_bytes());
    h.update(b"\x1f");

    match msgid.and_then(normalize_message_id) {
        Some(m) => {
            h.update(b"mid\x1f");
            h.update(m.as_bytes());
        }
        None => {
            h.update(b"syn\x1f");
            h.update(fallback.date.to_string().as_bytes());
            h.update(b"\x1f");
            h.update(fallback.from.as_bytes());
            h.update(b"\x1f");
            h.update(fallback.subject.as_bytes());
            h.update(b"\x1f");
            h.update(fallback.size.to_string().as_bytes());
        }
    }
    hex::encode(h.finalize().as_bytes())
}

/// Strips the angle brackets and surrounding whitespace, and rejects what is
/// left if it could not have come from a real `Message-ID`.
///
/// Kept verbatim otherwise — no case folding. Two devices see the same header
/// bytes, and RFC 5322 makes the local part case-sensitive, so normalizing case
/// would only invent differences where there were none.
pub fn normalize_message_id(raw: &str) -> Option<String> {
    let t = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    // Real-world junk seen in the wild: empty, a bare `<>`, a single dot, and
    // literal placeholder text from a broken generator.
    if t.len() < 3 || !t.contains('@') || t.contains(char::is_whitespace) {
        return None;
    }
    Some(t.to_string())
}

/// The id of a thread is the id of its root message (§5.6), so this is a rename
/// rather than a second hash — a thread whose root is fetched later must not
/// change id.
pub type ThreadId = String;

/// Shortens an id for display.
pub fn short(id: &str) -> &str {
    &id[..SHORT_LEN.min(id.len())]
}

/// The content address of a blob: BLAKE3 of the raw MIME bytes, which is what
/// makes a message stored once even when it appears in `INBOX` and
/// `[Gmail]/All Mail` (§5.1).
pub fn blob_id(raw: &[u8]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"mailbox/blob/v1\x1f");
    h.update(raw);
    hex::encode(h.finalize().as_bytes())
}

/// `blobs/ab/cdef…` — a two-character shard, so no directory holds a million
/// entries.
pub fn blob_path(blob_id: &str) -> String {
    format!("{}/{}", &blob_id[..2], &blob_id[2..])
}

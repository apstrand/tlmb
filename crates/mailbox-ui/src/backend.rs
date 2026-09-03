//! What the interface asks for, and what comes back.
//!
//! The interface never touches a store and never opens a socket. It hands a
//! [`Request`] to a [`Backend`] and, some frames later, collects a
//! [`Response`]. Everything that makes the desktop shell and the browser
//! different lives behind that line — the desktop's backend reads `meta.db` on
//! a worker thread, the browser's fetches from `mailbox serve` — and nothing
//! above it knows which it is talking to.
//!
//! The line is drawn there for a reason beyond tidiness. In a browser there are
//! no threads and no blocking calls; if the interface could block, it could not
//! be the same interface. So the contract is deliberately one a `fetch` can
//! satisfy: submit, carry on drawing, collect later.

use mailbox_api::{FolderList, MessageList, ShowResult, SyncResult};

/// Identifies a request, so a late answer to an old question can be discarded.
///
/// Over HTTP the answers can arrive in any order: type three characters into
/// the search box and the third listing may come back before the second. A
/// sequence number is what keeps the interface from showing results for a
/// query the user has already moved on from.
pub type Seq = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// The listing for a query — empty means the default inbox view.
    List {
        query: String,
        limit: usize,
    },
    /// One message, rendered.
    Open {
        id: String,
    },
    Folders,
    Flag {
        ids: Vec<String>,
        add: Vec<String>,
        remove: Vec<String>,
    },
    Move {
        ids: Vec<String>,
        folder: String,
    },
    /// `until` absent wakes a message instead of snoozing it (§7).
    Snooze {
        ids: Vec<String>,
        until: Option<String>,
    },
    Sync,
}

/// An answer. The error side is a string because it has already crossed a
/// process boundary in the web case, where a typed error would be a second
/// contract to keep in step with §6.5's.
///
/// The large variants are boxed: a `ShowResult` carries a whole message, and
/// without the box every response — including the one-line "changed" — would
/// be that size, in a queue the interface drains every frame.
#[derive(Debug, Clone)]
pub enum Response {
    List(Result<Box<MessageList>, String>),
    Opened(Result<Box<ShowResult>, String>),
    Folders(Result<Box<FolderList>, String>),
    /// A write finished. The string is what to say about it.
    Changed(Result<String, String>),
    Synced(Result<Box<SyncResult>, String>),
}

/// Where the mail comes from.
///
/// Neither method may block: `send` starts work, `poll` collects whatever has
/// finished. A backend that blocked in either would freeze the interface, and
/// in the browser it could not be written at all.
pub trait Backend {
    fn send(&mut self, seq: Seq, request: Request);

    /// The next finished response, or `None`. Called once per frame.
    fn poll(&mut self) -> Option<(Seq, Response)>;

    /// What this is connected to, for the status line: a store path, a URL.
    fn label(&self) -> String;
}

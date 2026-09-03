//! The interface's state, and every transition it can make.
//!
//! Free of egui, exactly as the TUI's state machine is free of ratatui, and for
//! the same payoff: §15 says M5's UI work is "covered through the same
//! CLI/JSON assertions rather than a screenshot suite", and this is what makes
//! that possible. Every rule §7 states about the GUI — a single search field
//! taking the §6.2 grammar, keyboard triage, multi-account invisible until you
//! want it — is checked here against a fake backend, in microseconds, with no
//! window.
//!
//! Nothing here blocks. Work goes out as a [`Request`] and comes back later,
//! which is what lets the same code run in a browser where blocking does not
//! exist.

use mailbox_api::{Folder, MessageDetail, MessageSummary};

use crate::backend::{Backend, Request, Response, Seq};

/// How many rows a listing asks for.
const PAGE: usize = 200;

/// Which pane the keyboard is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Search,
    List,
    Message,
}

pub struct App {
    /// The text in the single search field (§7). Not yet submitted.
    pub search: String,
    /// The query the current listing answers.
    pub query: String,
    pub messages: Vec<MessageSummary>,
    pub selected: Option<usize>,
    pub open: Option<MessageDetail>,
    pub folders: Vec<Folder>,
    pub status: String,
    /// Set when something failed, and shown until the next thing succeeds.
    pub error: Option<String>,
    pub focus: Focus,
    /// Requests sent and not yet answered, so the interface can say it is
    /// working without ever waiting.
    pub in_flight: usize,

    next_seq: Seq,
    /// The listing request whose answer is still wanted. An older one that
    /// arrives late is dropped rather than painted over a newer result.
    awaiting_list: Option<Seq>,
    awaiting_open: Option<Seq>,
}

impl Default for App {
    fn default() -> App {
        App::new()
    }
}

impl App {
    pub fn new() -> App {
        App {
            search: String::new(),
            query: String::new(),
            messages: Vec::new(),
            selected: None,
            open: None,
            folders: Vec::new(),
            status: String::new(),
            error: None,
            focus: Focus::List,
            in_flight: 0,
            next_seq: 0,
            awaiting_list: None,
            awaiting_open: None,
        }
    }

    /// The first load: the inbox and the folder list.
    pub fn start(&mut self, backend: &mut dyn Backend) {
        self.reload(backend);
        let seq = self.issue(backend, Request::Folders);
        let _ = seq;
    }

    pub fn selected_message(&self) -> Option<&MessageSummary> {
        self.selected.and_then(|i| self.messages.get(i))
    }

    pub fn selected_id(&self) -> Option<String> {
        self.selected_message().map(|m| m.id.clone())
    }

    /// Sends a request and counts it as outstanding.
    fn issue(&mut self, backend: &mut dyn Backend, request: Request) -> Seq {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.in_flight += 1;
        backend.send(seq, request);
        seq
    }

    /// Asks for the listing the current query implies.
    pub fn reload(&mut self, backend: &mut dyn Backend) {
        let seq = self.issue(
            backend,
            Request::List {
                query: self.query.clone(),
                limit: PAGE,
            },
        );
        self.awaiting_list = Some(seq);
    }

    /// Runs what is in the search box (§7: one field, the §6.2 grammar).
    pub fn submit_search(&mut self, backend: &mut dyn Backend) {
        self.query = self.search.trim().to_string();
        self.reload(backend);
    }

    /// Clears the search and goes back to the default listing.
    pub fn clear_search(&mut self, backend: &mut dyn Backend) {
        if self.search.is_empty() && self.query.is_empty() {
            return;
        }
        self.search.clear();
        self.query.clear();
        self.reload(backend);
    }

    /// Moves the selection, and opens what it lands on.
    ///
    /// Opening on selection is the difference between a list and a mail client:
    /// §7 asks for no modal waits, and a reading pane that fills in as the
    /// cursor moves is what that means here.
    pub fn select(&mut self, index: usize, backend: &mut dyn Backend) {
        if self.messages.is_empty() {
            self.selected = None;
            return;
        }
        let index = index.min(self.messages.len() - 1);
        if self.selected == Some(index) && self.open.is_some() {
            return;
        }
        self.selected = Some(index);
        self.open = None;
        if let Some(id) = self.selected_id() {
            let seq = self.issue(backend, Request::Open { id });
            self.awaiting_open = Some(seq);
        }
    }

    pub fn move_selection(&mut self, delta: isize, backend: &mut dyn Backend) {
        if self.messages.is_empty() {
            return;
        }
        let last = self.messages.len() as isize - 1;
        let next = match self.selected {
            Some(i) => (i as isize + delta).clamp(0, last),
            // Arriving from nowhere starts at the top whichever way you moved.
            None => 0,
        };
        self.select(next as usize, backend);
    }

    /// `is:unread` on the selection, toggled.
    pub fn toggle_read(&mut self, backend: &mut dyn Backend) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let seen = message.flags.iter().any(|f| f == "seen");
        let ids = vec![message.id.clone()];
        let request = if seen {
            Request::Flag {
                ids,
                add: Vec::new(),
                remove: vec!["seen".into()],
            }
        } else {
            Request::Flag {
                ids,
                add: vec!["seen".into()],
                remove: Vec::new(),
            }
        };
        self.issue(backend, request);
    }

    pub fn toggle_flagged(&mut self, backend: &mut dyn Backend) {
        let Some(message) = self.selected_message() else {
            return;
        };
        let flagged = message.flags.iter().any(|f| f == "flagged");
        let ids = vec![message.id.clone()];
        let request = if flagged {
            Request::Flag {
                ids,
                add: Vec::new(),
                remove: vec!["flagged".into()],
            }
        } else {
            Request::Flag {
                ids,
                add: vec!["flagged".into()],
                remove: Vec::new(),
            }
        };
        self.issue(backend, request);
    }

    /// §7's triage: archive, delete, spam. The selection stays where it is, so
    /// the next message moves under the cursor.
    pub fn move_to(&mut self, folder: &str, backend: &mut dyn Backend) {
        let Some(id) = self.selected_id() else { return };
        self.issue(
            backend,
            Request::Move {
                ids: vec![id],
                folder: folder.to_string(),
            },
        );
    }

    pub fn snooze(&mut self, until: &str, backend: &mut dyn Backend) {
        let Some(id) = self.selected_id() else { return };
        self.issue(
            backend,
            Request::Snooze {
                ids: vec![id],
                until: Some(until.to_string()),
            },
        );
    }

    pub fn sync(&mut self, backend: &mut dyn Backend) {
        self.issue(backend, Request::Sync);
    }

    /// Collects whatever finished. Called once per frame; never waits.
    ///
    /// Returns true when something arrived, so a shell that only repaints on
    /// change knows to.
    pub fn pump(&mut self, backend: &mut dyn Backend) -> bool {
        let mut changed = false;
        while let Some((seq, response)) = backend.poll() {
            self.in_flight = self.in_flight.saturating_sub(1);
            changed = true;
            self.apply(seq, response, backend);
        }
        changed
    }

    fn apply(&mut self, seq: Seq, response: Response, backend: &mut dyn Backend) {
        match response {
            Response::List(result) => {
                // A listing for a query the user has already moved past. Type
                // three characters quickly and the answers can arrive in any
                // order; painting a stale one is how a search box appears to
                // ignore what was typed.
                if self.awaiting_list != Some(seq) {
                    return;
                }
                self.awaiting_list = None;
                match result {
                    Ok(list) => {
                        self.error = None;
                        // Keep the cursor on the same message where one exists,
                        // so a refresh does not move the selection out from
                        // under someone mid-read.
                        let previous = self.selected_id();
                        self.messages = list.messages;
                        self.selected = previous
                            .and_then(|id| self.messages.iter().position(|m| m.id == id))
                            .or(if self.messages.is_empty() {
                                None
                            } else {
                                Some(0)
                            });
                        if self.open.is_none() {
                            if let Some(index) = self.selected {
                                self.selected = None;
                                self.select(index, backend);
                            }
                        }
                    }
                    Err(e) => self.error = Some(e),
                }
            }

            Response::Opened(result) => {
                if self.awaiting_open != Some(seq) {
                    return;
                }
                self.awaiting_open = None;
                match result {
                    Ok(shown) => {
                        self.error = None;
                        self.open = shown.message;
                    }
                    Err(e) => self.error = Some(e),
                }
            }

            Response::Folders(result) => match result {
                Ok(list) => self.folders = list.folders,
                Err(e) => self.error = Some(e),
            },

            // A write changed the store, so the listing on screen is stale.
            Response::Changed(result) => match result {
                Ok(status) => {
                    self.status = status;
                    self.error = None;
                    self.reload(backend);
                }
                Err(e) => self.error = Some(e),
            },

            Response::Synced(result) => match result {
                Ok(sync) => {
                    let added: u32 = sync.synced.iter().map(|r| r.messages_added).sum();
                    self.status = format!(
                        "synced — {added} new message{}",
                        if added == 1 { "" } else { "s" }
                    );
                    self.error = None;
                    self.reload(backend);
                }
                Err(e) => self.error = Some(format!("sync failed: {e}")),
            },
        }
    }
}

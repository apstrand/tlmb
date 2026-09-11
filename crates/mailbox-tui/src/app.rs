//! The interface's state, and every transition it can make.
//!
//! Deliberately free of ratatui and crossterm: this module takes [`Action`]s
//! and produces state, and the renderer reads that state. Two things follow.
//! A test can press `Action::Down` and assert on the result without a
//! terminal — which is how §7's behaviour is checked at all — and the §10
//! budget of 16 ms per keystroke is a property of code that never blocks,
//! rather than a hope about drawing.
//!
//! > Everything is incremental: the index list paints before sync completes;
//! > searching filters as you type; no modal "please wait" states, ever.
//!
//! That sentence is the whole design constraint. Nothing here waits on a
//! network, and nothing here waits on a sync. Work that could block is handed
//! to a worker thread and its result arrives as an [`Update`].

use mailbox_api::{MessageDetail, MessageSummary};
use mailbox_core::error::Result;
use mailbox_core::session::IdRef;
use mailbox_core::store::read::{ListOptions, Sort};
use mailbox_core::Workspace;

use crate::keys::Action;

/// Which pane has the user's attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The index: a list of messages.
    List,
    /// One message, opened.
    Message,
    /// The whole thread the selection belongs to.
    Thread,
    /// The open message's links, to pick one from.
    ///
    /// A separate view rather than a prompt asking for a number: the numbers
    /// mean nothing without the URLs beside them, and a reader deciding whether
    /// to follow a link in mail needs to see where it goes first (§9).
    Links,
    Help,
}

/// A line of text the user is typing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// `/` — filters the list on every keystroke (§7).
    Search(String),
    /// `t` — a tag to add.
    Tag(String),
    /// `|` — a command to pipe the raw message to.
    Pipe(String),
    /// `z` — when the message should come back (§7).
    Snooze(String),
}

impl Prompt {
    pub fn text(&self) -> &str {
        match self {
            Prompt::Search(t) | Prompt::Tag(t) | Prompt::Pipe(t) | Prompt::Snooze(t) => t,
        }
    }

    fn text_mut(&mut self) -> &mut String {
        match self {
            Prompt::Search(t) | Prompt::Tag(t) | Prompt::Pipe(t) | Prompt::Snooze(t) => t,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Prompt::Search(_) => "/",
            Prompt::Tag(_) => "tag:",
            Prompt::Pipe(_) => "|",
            Prompt::Snooze(_) => "snooze until:",
        }
    }
}

/// Something a worker finished, delivered back to the interface.
#[derive(Debug)]
pub enum Update {
    /// A sync pass ended. The list is reloaded from the store, because the
    /// store is what the list shows.
    SyncFinished(std::result::Result<String, String>),
    /// A long-running action reported something worth saying.
    Status(String),
}

/// What the caller should do after handling an action.
///
/// Returned rather than performed, because the things that block — running an
/// editor, piping to a command — need the terminal handed back to them, and
/// that is the shell's job rather than the state machine's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    /// Suspend the interface, run this, and come back. Used for `$EDITOR` and
    /// for §7's `|`.
    Suspend(SuspendedTask),
    /// Start a sync on a worker thread.
    StartSync,
    /// Hand a URL to the desktop's browser. Never automatic: this is only ever
    /// the result of someone selecting a link they can see (§9).
    OpenUrl(String),
    /// A store write, named rather than performed: the shell owns the session,
    /// as it does for the flag and move keys.
    Write(Write),
    /// Put this on the clipboard. The shell owns the terminal, and the
    /// clipboard is reached through it (OSC 52).
    Copy(String),
}

/// A write the interface asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// `t` — add a local tag (§5.4).
    Tag { id: String, tag: String },
    /// `z` — set aside until a time the shell parses, since the grammar for it
    /// lives in the core and the answer depends on the injected clock (§7).
    Snooze { id: String, until: String },
    /// Opening a message marks it read, as mutt does (§7).
    MarkRead { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuspendedTask {
    Reply { id: String, all: bool },
    Forward { id: String },
    Compose,
    Pipe { id: String, command: String },
}

/// How many rows a page-up or page-down moves.
///
/// Set by the renderer from the real pane height, so paging matches what the
/// user can see. A default for the case where nothing has been drawn yet.
const DEFAULT_PAGE: usize = 20;

pub struct App {
    pub view: View,
    /// The rows currently shown.
    pub messages: Vec<MessageSummary>,
    pub selected: usize,
    /// The opened message, when there is one.
    pub open: Option<MessageDetail>,
    /// How far the body is scrolled.
    pub body_scroll: u16,
    pub prompt: Option<Prompt>,
    /// The query the list is filtered by. Empty means the default listing.
    pub query: String,
    pub status: String,
    /// True while a worker is syncing — shown, never waited on.
    pub syncing: bool,
    pub page_size: usize,
    /// Set when the list no longer matches the store and should be reloaded.
    pub dirty: bool,
    /// True when nothing is configured yet, so the index draws §9's setup
    /// guide instead of a listing.
    ///
    /// Kept apart from "the list is empty" because the two want opposite
    /// answers: an empty inbox is good news and wants a sync, and an
    /// unconfigured mailbox wants a command typed in another window. Saying
    /// "no messages" for the second is how a working program looks broken.
    pub no_accounts: bool,
    /// Which link is highlighted in [`View::Links`].
    pub link_selected: usize,
    /// Where `?` was pressed, so closing the help goes back there.
    ///
    /// Help is an overlay on whatever you were doing. Dropping you at the list
    /// afterwards loses the message you were reading, which is the specific
    /// annoyance of asking what a key does *while* using it.
    help_return: View,
}

impl App {
    pub fn new() -> App {
        App {
            view: View::List,
            messages: Vec::new(),
            selected: 0,
            open: None,
            body_scroll: 0,
            prompt: None,
            query: String::new(),
            status: String::new(),
            syncing: false,
            page_size: DEFAULT_PAGE,
            dirty: false,
            no_accounts: false,
            link_selected: 0,
            help_return: View::List,
        }
    }

    /// The selected message's id, if it is unread.
    fn unread_selection(&self) -> Option<String> {
        let message = self.current()?;
        (!message.flags.iter().any(|f| f == "seen")).then(|| message.id.clone())
    }

    /// The links in the open message, in the numbering its text uses.
    pub fn links(&self) -> &[String] {
        self.open
            .as_ref()
            .and_then(|m| m.body.as_ref())
            .map(|b| b.links.as_slice())
            .unwrap_or_default()
    }

    /// The message under the cursor.
    pub fn current(&self) -> Option<&MessageSummary> {
        self.messages.get(self.selected)
    }

    pub fn current_id(&self) -> Option<String> {
        self.current().map(|m| m.id.clone())
    }

    /// Reloads the list from the store.
    ///
    /// Reads only — §4.1's fast path — so it never contends with a sync in
    /// progress, which is what lets the list repaint while one is running.
    /// Drops back to the listing, forgetting whatever was open.
    ///
    /// Used when a message turns out not to exist any more: staying in a
    /// message view with nothing in it would leave the interface showing the
    /// previous message's body under the missing one's name.
    pub fn back_to_list(&mut self) {
        self.view = View::List;
        self.open = None;
        self.body_scroll = 0;
    }

    pub fn reload(&mut self, workspace: &Workspace) -> Result<()> {
        // Read from the workspace on every reload rather than latched at
        // startup: the interface stays up while an account is added elsewhere,
        // and the guide has to come down again when one is.
        self.no_accounts = workspace.is_empty();

        let opts = ListOptions {
            limit: 500,
            sort: if self.query.is_empty() {
                Sort::UnreadFirst
            } else {
                // A search is answered newest-first: unread-first would bury the
                // match the user was looking for.
                Sort::DateDesc
            },
            ..Default::default()
        };
        let list = if self.query.is_empty() {
            workspace.ls(None, &opts)?
        } else {
            workspace.search(&self.query, &opts)?
        };

        // Keep the cursor on the same message where possible: a reload after a
        // sync should not move the selection out from under someone.
        let previous = self.current_id();
        self.messages = list.messages;
        self.selected = previous
            .and_then(|id| self.messages.iter().position(|m| m.id == id))
            .unwrap_or_else(|| self.selected.min(self.messages.len().saturating_sub(1)));
        self.dirty = false;
        Ok(())
    }

    /// Feeds a keystroke to whatever is asking for text.
    ///
    /// Returns true when the key was consumed by a prompt, so the caller knows
    /// not to look it up in the keymap.
    pub fn type_into_prompt(&mut self, c: char) -> bool {
        match &mut self.prompt {
            Some(p) => {
                p.text_mut().push(c);
                true
            }
            None => false,
        }
    }

    pub fn backspace_prompt(&mut self) -> bool {
        match &mut self.prompt {
            Some(p) => {
                p.text_mut().pop();
                true
            }
            None => false,
        }
    }

    /// Cancels whatever is being typed.
    pub fn cancel_prompt(&mut self) {
        // A cancelled search restores the unfiltered list, which is what
        // "cancel" has to mean or `/` becomes a trap.
        if matches!(self.prompt, Some(Prompt::Search(_))) && !self.query.is_empty() {
            self.query.clear();
            self.dirty = true;
        }
        self.prompt = None;
    }

    /// Accepts what was typed. Returns an effect for prompts that need one.
    pub fn submit_prompt(&mut self) -> Effect {
        let Some(prompt) = self.prompt.take() else {
            return Effect::None;
        };
        match prompt {
            // The list is already filtered — searching happens as you type —
            // so Enter only dismisses the prompt.
            Prompt::Search(_) => Effect::None,
            Prompt::Tag(tag) if !tag.trim().is_empty() => match self.current_id() {
                Some(id) => Effect::Write(Write::Tag {
                    id,
                    tag: tag.trim().to_string(),
                }),
                None => Effect::None,
            },
            Prompt::Tag(_) => Effect::None,
            Prompt::Snooze(until) if !until.trim().is_empty() => match self.target_id() {
                Some(id) => Effect::Write(Write::Snooze {
                    id,
                    until: until.trim().to_string(),
                }),
                None => Effect::None,
            },
            Prompt::Snooze(_) => Effect::None,
            Prompt::Pipe(command) if !command.trim().is_empty() => match self.current_id() {
                Some(id) => Effect::Suspend(SuspendedTask::Pipe {
                    id,
                    command: command.trim().to_string(),
                }),
                None => Effect::None,
            },
            Prompt::Pipe(_) => Effect::None,
        }
    }

    /// The live search text, if a search prompt is open.
    pub fn live_search(&self) -> Option<&str> {
        match &self.prompt {
            Some(Prompt::Search(text)) => Some(text),
            _ => None,
        }
    }

    /// Applies an action.
    ///
    /// Nothing here blocks. Anything that would — a sync, an editor, a pipe —
    /// leaves as an [`Effect`] for the shell to run, which is what keeps §10's
    /// 16 ms repaint budget a property of the code rather than a wish.
    pub fn handle(&mut self, action: Action) -> Effect {
        match action {
            Action::Quit => match self.view {
                // `q` in a sub-view goes back rather than quitting: losing a
                // session because you wanted to close a message is the classic
                // way to make people distrust a TUI.
                View::Message | View::Thread | View::Links | View::Help => {
                    self.back();
                    Effect::None
                }
                View::List if self.prompt.is_some() => {
                    self.cancel_prompt();
                    Effect::None
                }
                View::List => Effect::Quit,
            },

            Action::Back => {
                if self.prompt.is_some() {
                    self.cancel_prompt();
                } else {
                    self.back();
                }
                Effect::None
            }

            Action::Down => {
                self.move_by(1);
                Effect::None
            }
            Action::Up => {
                self.move_by(-1);
                Effect::None
            }
            Action::PageDown => {
                self.move_by(self.page_size as isize);
                Effect::None
            }
            Action::PageUp => {
                self.move_by(-(self.page_size as isize));
                Effect::None
            }
            Action::Top => {
                self.selected = 0;
                self.body_scroll = 0;
                Effect::None
            }
            Action::Bottom => {
                self.selected = self.messages.len().saturating_sub(1);
                Effect::None
            }

            Action::ScrollDown => {
                self.body_scroll = self.body_scroll.saturating_add(1);
                Effect::None
            }
            Action::ScrollUp => {
                self.body_scroll = self.body_scroll.saturating_sub(1);
                Effect::None
            }

            Action::Open => {
                if self.view == View::Links {
                    return match self.links().get(self.link_selected) {
                        Some(url) => Effect::OpenUrl(url.clone()),
                        None => Effect::None,
                    };
                }
                if self.view == View::List && self.current().is_some() {
                    self.view = View::Message;
                    self.body_scroll = 0;
                    // Opening a message is reading it, which is what mutt does
                    // and what §7 asks this interface to feel like. No dwell is
                    // needed: `j` and `k` move the cursor without opening
                    // anything, so this only ever fires on a deliberate act.
                    if let Some(id) = self.unread_selection() {
                        return Effect::Write(Write::MarkRead { id });
                    }
                }
                Effect::None
            }
            Action::Thread => {
                if self.current().is_some() {
                    self.view = View::Thread;
                }
                Effect::None
            }
            // `?` toggles: pressing it again closes the help, which is what
            // anyone who opened it with `?` reaches for first.
            Action::Help => {
                if self.view == View::Help {
                    self.back();
                } else {
                    self.help_return = self.view;
                    self.view = View::Help;
                }
                Effect::None
            }

            // A message with no links says so rather than opening an empty
            // pane — an empty list looks like a failure to load.
            Action::Links => {
                if self.links().is_empty() {
                    self.status = "no links in this message".to_string();
                } else {
                    self.link_selected = 0;
                    self.view = View::Links;
                }
                Effect::None
            }

            Action::Search => {
                self.prompt = Some(Prompt::Search(String::new()));
                self.view = View::List;
                Effect::None
            }
            Action::Tag => {
                if self.current().is_some() {
                    self.prompt = Some(Prompt::Tag(String::new()));
                }
                Effect::None
            }
            Action::Pipe => {
                if self.current().is_some() {
                    self.prompt = Some(Prompt::Pipe(String::new()));
                }
                Effect::None
            }
            Action::Snooze => {
                if self.current().is_some() {
                    self.prompt = Some(Prompt::Snooze(String::new()));
                }
                Effect::None
            }

            // Only in the picker, which is the only place with a link
            // selected. Elsewhere it would have to guess which link was meant.
            Action::Yank => match (self.view, self.links().get(self.link_selected)) {
                (View::Links, Some(url)) => Effect::Copy(url.clone()),
                _ => {
                    self.status = "press o first, to pick a link".to_string();
                    Effect::None
                }
            },

            Action::Sync => Effect::StartSync,

            Action::Reply | Action::ReplyAll | Action::Forward => match self.current_id() {
                Some(id) => Effect::Suspend(match action {
                    Action::Reply => SuspendedTask::Reply { id, all: false },
                    Action::ReplyAll => SuspendedTask::Reply { id, all: true },
                    _ => SuspendedTask::Forward { id },
                }),
                None => Effect::None,
            },
            Action::Compose => Effect::Suspend(SuspendedTask::Compose),

            // The store writes are performed by the shell, which owns the
            // session; the state machine only says what was asked for.
            Action::ToggleRead
            | Action::ToggleFlagged
            | Action::Archive
            | Action::Delete
            | Action::Spam => Effect::None,
        }
    }

    fn back(&mut self) {
        match self.view {
            // The picker was opened from the message, so it goes back to it —
            // closing the mail because you looked at its links and changed your
            // mind would lose your place.
            View::Links => {
                self.view = View::Message;
            }
            // The help overlays whatever was underneath it, and leaves it
            // there.
            View::Help => {
                self.view = self.help_return;
            }
            View::Message | View::Thread => {
                self.view = View::List;
                self.open = None;
                self.body_scroll = 0;
            }
            View::List => {
                // Backing out of a filtered list clears the filter before it
                // does anything else.
                if !self.query.is_empty() {
                    self.query.clear();
                    self.dirty = true;
                }
            }
        }
    }

    fn move_by(&mut self, delta: isize) {
        // In the link picker the cursor moves over links, not messages: the
        // same keys, because they are the same gesture.
        if self.view == View::Links {
            let links = self.links().len();
            if links == 0 {
                self.link_selected = 0;
                return;
            }
            let next = self.link_selected as isize + delta;
            self.link_selected = next.clamp(0, links as isize - 1) as usize;
            return;
        }
        if self.messages.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.messages.len() - 1;
        let next = self.selected as isize + delta;
        // Clamped rather than wrapping: `j` at the bottom of a list should stop,
        // not teleport to the top.
        self.selected = next.clamp(0, last as isize) as usize;
        self.body_scroll = 0;
    }

    /// Which id the flag and move actions should act on.
    ///
    /// The opened message when one is open, otherwise the selection — so `d`
    /// deletes what is on screen rather than what the cursor happens to be on.
    pub fn target(&self) -> Option<IdRef> {
        self.target_id().map(IdRef::Id)
    }

    /// The same choice as [`App::target`], as a plain id.
    pub fn target_id(&self) -> Option<String> {
        match (&self.open, self.view) {
            (Some(detail), View::Message) => Some(detail.summary.id.clone()),
            _ => self.current_id(),
        }
    }
}

impl Default for App {
    fn default() -> App {
        App::new()
    }
}

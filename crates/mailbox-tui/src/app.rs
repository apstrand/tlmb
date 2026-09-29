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

/// Which of the three panes the keys act on while the index is showing (§7).
///
/// The terminal does not hold all three open the way the window does — a
/// terminal is usually narrower, and a folder list permanently taking a
/// quarter of it is a quarter of the subjects gone. They slide in when asked
/// for and out again when done with, so the common case is the list alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Folders,
    List,
}

/// What is open beside the message list, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    None,
    Folders,
    Preview,
}

/// One row of the folder pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    /// The account this row is for, or `None` for one that spans them all.
    pub account: Option<String>,
    /// The path the server uses, which is what a query has to name.
    pub path: String,
    /// What to show.
    pub label: String,
    pub unread: u32,
    pub total: u32,
    /// Drawn under the row above, which is the row it belongs to.
    pub indent: bool,
}

impl FolderRow {
    /// The query that shows this folder, quoted when the path needs it.
    ///
    /// A row spanning accounts asks for the folder by name, which without an
    /// `account:` beside it is every account's copy at once (§6.2).
    pub fn query(&self) -> String {
        let path = if self.path.contains(' ') {
            format!("\"{}\"", self.path)
        } else {
            self.path.clone()
        };
        match &self.account {
            Some(account) => format!("account:{account} in:{path}"),
            None => format!("in:{path}"),
        }
    }
}

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

/// `inbox` as a label rather than a protocol word.
fn title_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A reversible move kept only for this TUI session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub id: String,
    pub account: String,
    pub folder: String,
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
    /// The most recent destructive moves, retained until this session ends.
    pub undo: Vec<UndoEntry>,
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
    /// The folders, for the pane that slides in from the left.
    pub folders: Vec<FolderRow>,
    pub folder_selected: usize,
    /// Which pane is open beside the list, if either.
    ///
    /// Never both. A terminal showing folders, list and message at once is
    /// three narrow columns; and the two are wanted at different moments —
    /// one to choose what to read, the other to read it.
    pub side: Side,
    /// Which pane the keys act on.
    pub focus: Focus,
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
            undo: Vec::new(),
            page_size: DEFAULT_PAGE,
            dirty: false,
            no_accounts: false,
            link_selected: 0,
            folders: Vec::new(),
            folder_selected: 0,
            side: Side::None,
            focus: Focus::List,
            help_return: View::List,
        }
    }

    /// The folder under the folder cursor.
    pub fn current_folder(&self) -> Option<&FolderRow> {
        self.folders.get(self.folder_selected)
    }

    /// Reads the folder pane's contents from the workspace.
    ///
    /// Inbox first, then the rest by account and path, so the folder someone
    /// wants is the one the cursor starts on.
    pub fn load_folders(&mut self, workspace: &Workspace) -> Result<()> {
        let listing = workspace.folders()?;

        // Grouped by what they are called, not by which account has them:
        // "Inbox" is one idea that three accounts happen to implement, and a
        // flat list of it is three rows reading "Inbox".
        let mut groups: Vec<(String, Vec<mailbox_api::Folder>)> = Vec::new();
        for folder in listing.folders.into_iter().filter(|f| f.selectable) {
            let label = match folder.special_use.as_deref() {
                Some(use_name) => title_case(use_name),
                None => folder.name.clone(),
            };
            match groups.iter_mut().find(|(name, _)| *name == label) {
                Some((_, members)) => members.push(folder),
                None => groups.push((label, vec![folder])),
            }
        }

        // Inbox first — it is where the mail someone is waiting for arrives —
        // and trash last, being where mail goes to be forgotten. The rest by
        // name between them. The window orders its nav the same way.
        groups.sort_by(|a, b| {
            let rank = |name: &str| match name {
                n if n.eq_ignore_ascii_case("inbox") => 0,
                n if n.eq_ignore_ascii_case("trash") => 2,
                _ => 1,
            };
            rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0))
        });

        let mut rows = Vec::new();
        for (label, mut members) in groups {
            members.sort_by(|a, b| a.account.cmp(&b.account).then_with(|| a.path.cmp(&b.path)));
            if members.len() == 1 {
                // Only one account has it, so there is nothing to span and a
                // parent row would be a heading over a single item.
                let folder = &members[0];
                rows.push(FolderRow {
                    account: Some(folder.account.clone()),
                    path: folder.path.clone(),
                    label,
                    unread: folder.unread,
                    total: folder.total,
                    indent: false,
                });
                continue;
            }
            // The row that spans them, then the accounts under it. The parent
            // asks for the folder by name, which without an `account:` beside
            // it is every account's copy at once (§6.2) — the unified view.
            rows.push(FolderRow {
                account: None,
                path: members[0].path.clone(),
                label: format!("{label} (unified)"),
                unread: members.iter().map(|f| f.unread).sum(),
                total: members.iter().map(|f| f.total).sum(),
                indent: false,
            });
            for folder in members {
                rows.push(FolderRow {
                    label: folder.account.clone(),
                    account: Some(folder.account),
                    path: folder.path,
                    unread: folder.unread,
                    total: folder.total,
                    indent: true,
                });
            }
        }

        self.folders = rows;
        self.folder_selected = self
            .folder_selected
            .min(self.folders.len().saturating_sub(1));
        Ok(())
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

            // The panes slide: left opens the folders or closes the message,
            // right shows the message beside the list and then focuses it.
            // Nothing here is modal — the list stays where it was, and every
            // one of these is reversible with the opposite arrow.
            // Left walks out of a pane; right walks in, and then through.
            // Neither is modal and neither can strand you: from the list,
            // left is always the folders and right is always the message.
            Action::PaneLeft => {
                if self.view != View::List {
                    self.back();
                    return Effect::None;
                }
                match self.side {
                    // Already as far left as there is. Doing nothing is the
                    // honest answer — closing the folders here would make the
                    // same key mean "open" and "close" one press apart.
                    Side::Folders => {}
                    // Reading beside the list: close it and come back.
                    Side::Preview => {
                        self.side = Side::None;
                        self.open = None;
                        self.body_scroll = 0;
                        self.focus = Focus::List;
                    }
                    Side::None => {
                        self.side = Side::Folders;
                        self.focus = Focus::Folders;
                        // The list already shows the folder under the cursor,
                        // so opening the pane shows what it is pointing at.
                        return self.show_selected_folder();
                    }
                }
                Effect::None
            }
            Action::PaneRight => {
                if self.view != View::List {
                    return Effect::None;
                }
                match self.side {
                    // The list has been following the cursor all along, so
                    // leaving the pane is all that is left to do.
                    Side::Folders => {
                        self.side = Side::None;
                        self.focus = Focus::List;
                        Effect::None
                    }
                    // Beside the list, then instead of it.
                    Side::Preview => {
                        self.side = Side::None;
                        self.view = View::Message;
                        self.body_scroll = 0;
                        match self.unread_selection() {
                            Some(id) => Effect::Write(Write::MarkRead { id }),
                            None => Effect::None,
                        }
                    }
                    Side::None if self.current().is_some() => {
                        self.side = Side::Preview;
                        self.focus = Focus::List;
                        self.body_scroll = 0;
                        Effect::None
                    }
                    Side::None => Effect::None,
                }
            }

            // Tab crosses to the other pane when there is one to cross to.
            Action::FocusNext => {
                if self.view == View::List && self.side == Side::Folders {
                    self.focus = match self.focus {
                        Focus::Folders => Focus::List,
                        Focus::List => Focus::Folders,
                    };
                }
                Effect::None
            }

            Action::Down => {
                self.step_by(1);
                self.follow_folder_cursor()
            }
            Action::Up => {
                self.step_by(-1);
                self.follow_folder_cursor()
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
                self.scroll_body(1);
                Effect::None
            }
            Action::ScrollUp => {
                self.scroll_body(-1);
                Effect::None
            }

            Action::Open => {
                if self.view == View::Links {
                    return match self.links().get(self.link_selected) {
                        Some(url) => Effect::OpenUrl(url.clone()),
                        None => Effect::None,
                    };
                }
                // Enter in the folders is done choosing: the list is already
                // showing them, so this puts the pane away and hands the keys
                // back to the messages.
                if self.view == View::List && self.focus == Focus::Folders {
                    self.side = Side::None;
                    self.focus = Focus::List;
                    return Effect::None;
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

            Action::MarkRead => self.mark_read_and_advance(),

            // The store writes are performed by the shell, which owns the
            // session; the state machine only says what was asked for.
            Action::ToggleRead
            | Action::ToggleFlagged
            | Action::Undo
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

    fn scroll_body(&mut self, delta: isize) {
        self.body_scroll = if delta < 0 {
            self.body_scroll.saturating_sub(delta.unsigned_abs() as u16)
        } else {
            self.body_scroll.saturating_add(delta as u16)
        };
    }

    /// Re-points the list when the move was one through the folders.
    fn follow_folder_cursor(&mut self) -> Effect {
        if self.view == View::List && self.focus == Focus::Folders {
            return self.show_selected_folder();
        }
        Effect::None
    }

    /// One step, in whichever pane the keys are pointed at.
    fn step_by(&mut self, delta: isize) {
        if self.view == View::List && self.focus == Focus::Folders {
            if self.folders.is_empty() {
                self.folder_selected = 0;
                return;
            }
            let next = self.folder_selected as isize + delta;
            self.folder_selected = next.clamp(0, self.folders.len() as isize - 1) as usize;
            return;
        }
        self.move_by(delta);
    }

    /// Points the message list at the folder under the cursor.
    ///
    /// Run on every move rather than on a keypress that commits: moving down
    /// the folders *is* choosing between them, and a list that only catches
    /// up when you press something is a list you cannot browse with.
    fn show_selected_folder(&mut self) -> Effect {
        let Some(folder) = self.current_folder() else {
            return Effect::None;
        };
        let query = folder.query();
        if query == self.query {
            return Effect::None;
        }
        self.query = query;
        self.selected = 0;
        self.open = None;
        self.dirty = true;
        Effect::None
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

    /// True when the list only shows unread mail, so that marking a message
    /// read takes it out of the list.
    pub fn hides_read(&self) -> bool {
        self.query.split_whitespace().any(|term| {
            term.eq_ignore_ascii_case("is:unread") || term.eq_ignore_ascii_case("is:unseen")
        })
    }

    /// `w`: done with this one for now, on to the next.
    ///
    /// Back to the list when a message was open, since the next one is found
    /// there. The cursor steps down unless the list is filtered to unread
    /// mail: there the message leaves the list when it is read, the next one
    /// moves up into its row, and stepping as well would skip it.
    fn mark_read_and_advance(&mut self) -> Effect {
        let Some(id) = self.target_id() else {
            return Effect::None;
        };
        let unread = self
            .messages
            .iter()
            .find(|m| m.id == id)
            .or(self.open.as_ref().map(|detail| &detail.summary))
            .is_some_and(|m| !m.flags.iter().any(|f| f == "seen"));
        if self.view == View::Message {
            self.back_to_list();
        }
        if !self.hides_read() {
            self.move_by(1);
        }
        if unread {
            Effect::Write(Write::MarkRead { id })
        } else {
            Effect::None
        }
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

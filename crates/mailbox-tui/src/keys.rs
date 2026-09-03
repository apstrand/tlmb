//! Keybindings (§7, §12).
//!
//! > Vim-ish by default, with an Emacs keymap and a fully remappable
//! > `keys.toml`.
//!
//! Keys map to [`Action`]s, and the rest of the interface only ever sees
//! actions. That is what makes the whole state machine testable without a
//! terminal — a test presses `Action::Down`, not a byte sequence — and what
//! makes remapping a lookup-table change rather than a rewrite.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Everything the interface can be asked to do.
///
/// Named for the intent rather than the key, so a rebinding reads as a
/// rebinding: `archive = "e"` and not `e = "move-to-archive"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Quit,
    /// Leave the current view — the message, the search — without quitting.
    Back,
    Down,
    Up,
    PageDown,
    PageUp,
    Top,
    Bottom,
    /// Open the selected message.
    Open,
    /// Start filtering as you type (§7's `/`).
    Search,
    /// Show the whole thread the selection belongs to.
    Thread,
    ToggleRead,
    ToggleFlagged,
    Tag,
    Archive,
    Delete,
    Spam,
    Reply,
    ReplyAll,
    Forward,
    Compose,
    /// Pipe the raw message to a command (§7's `|`).
    Pipe,
    /// List the open message's links, to follow one.
    Links,
    /// Set the message aside until later (§7).
    Snooze,
    /// Copy the selected link to the clipboard.
    Yank,
    Sync,
    Help,
    /// Scroll a message body without moving the selection.
    ScrollDown,
    ScrollUp,
}

impl Action {
    /// A one-line description, for the help view.
    pub fn describe(self) -> &'static str {
        match self {
            Action::Quit => "quit",
            Action::Back => "back",
            Action::Down => "next message",
            Action::Up => "previous message",
            Action::PageDown => "page down",
            Action::PageUp => "page up",
            Action::Top => "first message",
            Action::Bottom => "last message",
            Action::Open => "open",
            Action::Search => "search",
            Action::Thread => "show thread",
            Action::ToggleRead => "toggle read",
            Action::ToggleFlagged => "toggle flagged",
            Action::Tag => "tag",
            Action::Archive => "archive",
            Action::Delete => "delete",
            Action::Spam => "mark spam",
            Action::Reply => "reply",
            Action::ReplyAll => "reply all",
            Action::Forward => "forward",
            Action::Compose => "compose",
            Action::Pipe => "pipe to a command",
            Action::Links => "follow a link",
            Action::Snooze => "snooze until later",
            Action::Yank => "copy the selected link",
            Action::Sync => "sync now",
            Action::Help => "help",
            Action::ScrollDown => "scroll down",
            Action::ScrollUp => "scroll up",
        }
    }
}

/// A key as a string: `j`, `Enter`, `C-n`, `Esc`.
///
/// Text rather than a crossterm type so `keys.toml` is writable by hand and
/// this module does not put a terminal library in the config's grammar.
pub type Key = String;

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<Key, Action>,
}

impl Keymap {
    /// §7's default: mutt-like muscle memory, vim-ish movement.
    pub fn vim() -> Keymap {
        Keymap::from_pairs(&[
            ("q", Action::Quit),
            ("Esc", Action::Back),
            ("h", Action::Back),
            ("j", Action::Down),
            ("k", Action::Up),
            ("Down", Action::Down),
            ("Up", Action::Up),
            ("C-f", Action::PageDown),
            ("C-b", Action::PageUp),
            ("PageDown", Action::PageDown),
            ("PageUp", Action::PageUp),
            ("g", Action::Top),
            ("G", Action::Bottom),
            ("Home", Action::Top),
            ("End", Action::Bottom),
            ("Enter", Action::Open),
            ("l", Action::Open),
            ("/", Action::Search),
            ("T", Action::Thread),
            ("N", Action::ToggleRead),
            ("F", Action::ToggleFlagged),
            ("t", Action::Tag),
            ("s", Action::Archive),
            ("d", Action::Delete),
            ("S", Action::Spam),
            ("r", Action::Reply),
            ("R", Action::ReplyAll),
            ("f", Action::Forward),
            ("m", Action::Compose),
            ("|", Action::Pipe),
            ("o", Action::Links),
            ("z", Action::Snooze),
            ("y", Action::Yank),
            ("@", Action::Sync),
            ("?", Action::Help),
            ("C-d", Action::ScrollDown),
            ("C-u", Action::ScrollUp),
            (" ", Action::ScrollDown),
        ])
    }

    /// §7's alternative for people whose fingers expect it.
    ///
    /// Only movement and cancellation differ; the mutt-derived letters are the
    /// muscle memory the spec asks for and are the same in both.
    pub fn emacs() -> Keymap {
        let mut map = Keymap::vim();
        for key in ["h", "j", "k", "l", "g", "G", "C-f", "C-b", "C-d", "C-u"] {
            map.bindings.remove(key);
        }
        map.bind("C-n", Action::Down);
        map.bind("C-p", Action::Up);
        map.bind("C-v", Action::PageDown);
        map.bind("M-v", Action::PageUp);
        map.bind("M-<", Action::Top);
        map.bind("M->", Action::Bottom);
        map.bind("C-g", Action::Back);
        map.bind("C-s", Action::Search);
        map
    }

    fn from_pairs(pairs: &[(&str, Action)]) -> Keymap {
        Keymap {
            bindings: pairs.iter().map(|(k, a)| ((*k).to_string(), *a)).collect(),
        }
    }

    pub fn bind(&mut self, key: &str, action: Action) {
        self.bindings.insert(key.to_string(), action);
    }

    pub fn action(&self, key: &str) -> Option<Action> {
        self.bindings.get(key).copied()
    }

    /// Every key bound to an action, sorted, for the help view.
    pub fn keys_for(&self, action: Action) -> Vec<&str> {
        let mut keys: Vec<&str> = self
            .bindings
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(k, _)| k.as_str())
            .collect();
        keys.sort_unstable();
        keys
    }

    /// Applies `[keys]` from `config.toml` (§12).
    ///
    /// An override *replaces* every existing binding for that action rather
    /// than adding one. Someone who writes `archive = "e"` means archive is on
    /// `e`, not that it is now on both `e` and `s` — and leaving the old key
    /// live is how a rebinding silently fails to take effect.
    pub fn apply_overrides(
        &mut self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<String> {
        let mut unknown = Vec::new();
        for (name, key) in overrides {
            let Some(action) = action_by_name(name) else {
                unknown.push(name.clone());
                continue;
            };
            self.bindings.retain(|_, a| *a != action);
            self.bindings.insert(key.clone(), action);
        }
        unknown
    }

    /// Builds the keymap for a configuration: a named base plus overrides.
    pub fn from_config(config: &mailbox_core::config::Config) -> (Keymap, Vec<String>) {
        let mut map = match config.keys.get("keymap").map(String::as_str) {
            Some("emacs") => Keymap::emacs(),
            _ => Keymap::vim(),
        };
        let overrides: std::collections::BTreeMap<String, String> = config
            .keys
            .iter()
            // `keymap` selects the base; it is not an action.
            .filter(|(k, _)| k.as_str() != "keymap")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let unknown = map.apply_overrides(&overrides);
        (map, unknown)
    }
}

/// Looks up an action by its kebab-case config name.
pub fn action_by_name(name: &str) -> Option<Action> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

/// The name an action is written as in `keys.toml`.
pub fn action_name(action: Action) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Renders a crossterm key event as the text form the keymap uses.
///
/// Modifiers first and in a fixed order, so `C-M-x` is the only spelling and a
/// config file cannot miss a binding by writing them the other way round.
pub fn describe_key(event: &crossterm::event::KeyEvent) -> String {
    use crossterm::event::{KeyCode, KeyModifiers};

    let base = match event.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return String::new(),
    };

    let mut out = String::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("C-");
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        out.push_str("M-");
    }
    // Shift is not spelled out for characters: the character is already the
    // shifted one, and `S-G` would never match what a terminal reports.
    if event.modifiers.contains(KeyModifiers::SHIFT) && base.chars().count() > 1 {
        out.push_str("S-");
    }
    out.push_str(&base);
    out
}

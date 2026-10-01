//! §7 asks for one set of muscle memory, not two.
//!
//! The window and the terminal are different programs with different input
//! layers, so nothing makes their keymaps agree except someone checking. They
//! drifted: `s` archived in the window and marked read in the terminal, which
//! is the worst kind of difference — both do something, and the wrong one is
//! destructive.
//!
//! This reads the bindings out of the served `mailbox.js` and holds them
//! against `Keymap::vim`, the terminal's defaults, for the actions both have.

use mailbox_tui::{Action, Keymap};

/// The actions the window implements, and the binding it should carry.
///
/// Not every terminal action has a window equivalent — there is no `|` to
/// pipe to, and no tags yet — so the list is explicit rather than derived,
/// and the assertion below is that what *is* here matches.
const SHARED: &[Action] = &[
    Action::Down,
    Action::Up,
    Action::Top,
    Action::Bottom,
    Action::ToggleRead,
    Action::ToggleFlagged,
    Action::Archive,
    Action::Delete,
    Action::Spam,
    Action::Undo,
    Action::Reply,
    Action::ReplyAll,
    Action::Forward,
    Action::Compose,
    Action::Snooze,
    Action::Search,
    Action::Sync,
    Action::Help,
];

#[test]
fn the_window_and_the_terminal_agree_on_their_keys() {
    let script = include_str!("../../mailbox-web/web/mailbox.js");
    let bound = bindings(script);
    let keymap = Keymap::vim();

    let mut wrong = Vec::new();
    for action in SHARED {
        // The terminal's single-character bindings; the window has no notion
        // of `C-f`, and arrow keys it spells out for itself.
        let expected: Vec<String> = keymap
            .keys_for(*action)
            .into_iter()
            .filter(|k| k.chars().count() == 1)
            .map(str::to_string)
            .collect();
        if expected.is_empty() {
            continue;
        }
        for key in &expected {
            if !bound.contains(key) {
                wrong.push(format!(
                    "{} is `{key}` in the terminal and unbound in the window",
                    action.describe()
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "the keymaps have drifted:\n{}",
        wrong.join("\n")
    );
}

/// The keys that do destructive things must not have swapped meanings.
///
/// The general check above catches a missing binding; this catches the case
/// that actually happened, where both programs bound `s` and meant opposite
/// things by it.
///
/// The row `w a s d` is the triage layer: read-and-on, archive, spam, trash,
/// so an inbox can be worked through without the left hand moving. It
/// displaced flag and forward onto `f` and `F`, and toggle-read onto `W`.
/// Both programs took that change together, which is the point — the letters
/// here are what they agreed on, not what either of them started with.
#[test]
fn the_dangerous_letters_mean_what_the_terminal_means() {
    let script = include_str!("../../mailbox-web/web/mailbox.js");
    for (key, expected) in [
        ("w", "readAndNext"), // read it and move on
        ("a", "archive"),
        ("s", "junk"), // spam, not toggle-read — see the triage row above
        ("d", "trash"),
        ("W", "seen"),
        ("f", "flagged"),
        ("F", "forward"),
        ("u", "undoLast"),
    ] {
        let line =
            binding_line(script, key).unwrap_or_else(|| panic!("the window does not bind `{key}`"));
        assert!(
            line.contains(expected),
            "`{key}` should do {expected} as it does in the terminal, but the window has: {line}"
        );
    }
}

/// The keys named on the left of the `keys` object in `mailbox.js`.
fn bindings(script: &str) -> Vec<String> {
    let body = keys_object(script);
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if let Some(key) = line.strip_prefix('"').and_then(|r| r.split('"').next()) {
            out.push(key.to_string());
        } else if let Some((name, _)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty()
                && !name.starts_with("//")
                && name.chars().all(char::is_alphanumeric)
            {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// The whole entry for one key, so its body can be inspected.
fn binding_line(script: &str, key: &str) -> Option<String> {
    let body = keys_object(script);
    let mut lines = body.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let name = trimmed.split(':').next()?.trim();
        if name == key {
            // A binding may wrap onto the next line.
            let mut entry = trimmed.to_string();
            if let Some(next) = lines.peek() {
                if !next.trim().is_empty() && !next.contains(':') {
                    entry.push_str(next.trim());
                }
            }
            return Some(entry);
        }
    }
    None
}

fn keys_object(script: &str) -> &str {
    let start = script
        .find("const keys = {")
        .expect("mailbox.js has no `keys` object");
    let rest = &script[start..];
    let end = rest.find("\n};").expect("the `keys` object does not end");
    &rest[..end]
}

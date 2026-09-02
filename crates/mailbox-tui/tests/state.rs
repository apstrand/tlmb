//! The interface's behaviour (§7), tested by pressing actions rather than keys.
//!
//! This is the payoff for keeping [`App`] free of ratatui and crossterm: every
//! rule §7 states — clamped movement, `q` that does not lose your session, a
//! search prompt that eats keystrokes, cancelling that restores the list — is
//! checkable without a terminal, in microseconds.

use mailbox_api::{Address, MessageSummary};
use mailbox_tui::app::{App, Effect, Prompt, SuspendedTask};
use mailbox_tui::keys::{action_by_name, action_name, Action, Keymap};
use mailbox_tui::View;

fn message(id: &str, subject: &str, seen: bool) -> MessageSummary {
    MessageSummary {
        id: id.to_string(),
        account: "test".into(),
        folder: "INBOX".into(),
        thread_id: id.to_string(),
        date: "2026-09-01T12:00:00Z".into(),
        from: vec![Address::new(Some("Alice".into()), "alice@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        subject: subject.to_string(),
        flags: if seen {
            vec!["seen".to_string()]
        } else {
            Vec::new()
        },
        tags: Vec::new(),
        has_attachment: false,
        size: 100,
        preview: Some("preview".into()),
    }
}

fn app_with(count: usize) -> App {
    let mut app = App::new();
    app.messages = (0..count)
        .map(|i| message(&format!("id{i:02}"), &format!("Subject {i}"), i % 2 == 0))
        .collect();
    app
}

// ---- movement ----

#[test]
fn movement_is_clamped_not_wrapped() {
    let mut app = app_with(3);

    app.handle(Action::Up);
    assert_eq!(app.selected, 0, "up at the top should stay put");

    app.handle(Action::Down);
    app.handle(Action::Down);
    app.handle(Action::Down);
    app.handle(Action::Down);
    assert_eq!(
        app.selected, 2,
        "down at the bottom should stop, not teleport to the top"
    );
}

#[test]
fn paging_moves_by_the_visible_height() {
    let mut app = app_with(100);
    app.page_size = 10;

    app.handle(Action::PageDown);
    assert_eq!(app.selected, 10);
    app.handle(Action::PageUp);
    assert_eq!(app.selected, 0);

    app.handle(Action::Bottom);
    assert_eq!(app.selected, 99);
    app.handle(Action::Top);
    assert_eq!(app.selected, 0);
}

#[test]
fn movement_on_an_empty_list_is_harmless() {
    let mut app = App::new();
    for action in [
        Action::Down,
        Action::Up,
        Action::PageDown,
        Action::Bottom,
        Action::Open,
    ] {
        app.handle(action);
    }
    assert_eq!(app.selected, 0);
    assert_eq!(
        app.view,
        View::List,
        "opening nothing should not open a view"
    );
}

// ---- views ----

/// Losing a session because you wanted to close a message is the classic way to
/// make people distrust a TUI.
#[test]
fn quit_in_a_subview_goes_back_rather_than_quitting() {
    let mut app = app_with(3);
    app.handle(Action::Open);
    assert_eq!(app.view, View::Message);

    assert_eq!(
        app.handle(Action::Quit),
        Effect::None,
        "q should not quit here"
    );
    assert_eq!(app.view, View::List);

    assert_eq!(
        app.handle(Action::Quit),
        Effect::Quit,
        "q in the list quits"
    );
}

#[test]
fn back_leaves_a_message_and_then_clears_a_filter() {
    let mut app = app_with(3);
    app.query = "from:alice".into();
    app.handle(Action::Open);

    app.handle(Action::Back);
    assert_eq!(app.view, View::List);
    assert_eq!(
        app.query, "from:alice",
        "the filter survives closing a message"
    );

    app.handle(Action::Back);
    assert!(app.query.is_empty(), "a second back clears the filter");
    assert!(app.dirty, "and asks for a reload");
}

// ---- the search prompt ----

/// §7: "searching filters as you type".
#[test]
fn a_search_prompt_takes_text() {
    let mut app = app_with(3);
    app.handle(Action::Search);
    assert!(matches!(app.prompt, Some(Prompt::Search(_))));

    for c in "alice".chars() {
        assert!(app.type_into_prompt(c), "the prompt should take the key");
    }
    assert_eq!(app.live_search(), Some("alice"));

    app.backspace_prompt();
    assert_eq!(app.live_search(), Some("alic"));
}

/// Cancelling a search has to restore the list, or `/` is a trap.
#[test]
fn cancelling_a_search_restores_the_list() {
    let mut app = app_with(3);
    app.handle(Action::Search);
    app.type_into_prompt('x');
    app.query = "x".into();
    app.dirty = false;

    app.handle(Action::Back);
    assert!(app.prompt.is_none());
    assert!(app.query.is_empty(), "the filter should be gone");
    assert!(app.dirty, "and the list reloaded");
}

/// Enter only dismisses the prompt: the list is already filtered, because
/// filtering happens as you type.
#[test]
fn submitting_a_search_only_closes_the_prompt() {
    let mut app = app_with(3);
    app.handle(Action::Search);
    app.type_into_prompt('a');
    app.query = "a".into();

    assert_eq!(app.submit_prompt(), Effect::None);
    assert!(app.prompt.is_none());
    assert_eq!(app.query, "a", "the filter stays");
}

#[test]
fn a_pipe_prompt_produces_a_suspended_task() {
    let mut app = app_with(3);
    app.handle(Action::Pipe);
    for c in "less".chars() {
        app.type_into_prompt(c);
    }

    match app.submit_prompt() {
        Effect::Suspend(SuspendedTask::Pipe { id, command }) => {
            assert_eq!(id, "id00");
            assert_eq!(command, "less");
        }
        other => panic!("expected a pipe task, got {other:?}"),
    }
}

#[test]
fn an_empty_prompt_does_nothing() {
    let mut app = app_with(3);
    app.handle(Action::Pipe);
    assert_eq!(app.submit_prompt(), Effect::None);

    app.handle(Action::Tag);
    assert_eq!(app.submit_prompt(), Effect::None);
}

// ---- effects ----

#[test]
fn reply_and_forward_name_the_selected_message() {
    let mut app = app_with(3);
    app.handle(Action::Down);

    assert_eq!(
        app.handle(Action::Reply),
        Effect::Suspend(SuspendedTask::Reply {
            id: "id01".into(),
            all: false
        })
    );
    assert_eq!(
        app.handle(Action::ReplyAll),
        Effect::Suspend(SuspendedTask::Reply {
            id: "id01".into(),
            all: true
        })
    );
    assert_eq!(
        app.handle(Action::Forward),
        Effect::Suspend(SuspendedTask::Forward { id: "id01".into() })
    );
}

#[test]
fn sync_is_an_effect_rather_than_a_wait() {
    let mut app = app_with(3);
    // §7 forbids modal "please wait" states, so the state machine hands the
    // sync out and carries on rather than blocking on it.
    assert_eq!(app.handle(Action::Sync), Effect::StartSync);
    assert_eq!(app.view, View::List, "the interface stays usable");
}

/// `d` should delete what is on screen, not what the cursor happens to be on.
#[test]
fn actions_target_the_open_message_when_one_is_open() {
    let mut app = app_with(3);
    app.handle(Action::Down);
    assert_eq!(
        app.target(),
        Some(mailbox_core::session::IdRef::Id("id01".into()))
    );

    app.handle(Action::Open);
    let mut detail: mailbox_api::MessageDetail = serde_json::from_value(serde_json::json!({
        "id": "id01", "account": "test", "folder": "INBOX", "thread_id": "id01",
        "date": "2026-09-01T12:00:00Z", "from": [], "to": [], "cc": [],
        "subject": "Subject 1", "flags": [], "tags": [], "has_attachment": false,
        "size": 100, "references": [], "reply_to": [], "bcc": [], "headers": [],
        "attachments": []
    }))
    .expect("detail");
    detail.summary.id = "id01".into();
    app.open = Some(detail);
    assert_eq!(
        app.target(),
        Some(mailbox_core::session::IdRef::Id("id01".into()))
    );
}

// ---- keymap ----

#[test]
fn the_default_keymap_is_the_muscle_memory_the_spec_names() {
    let map = Keymap::vim();
    // §7 names these explicitly.
    for (key, action) in [
        ("j", Action::Down),
        ("k", Action::Up),
        ("/", Action::Search),
        ("t", Action::Tag),
        ("d", Action::Delete),
        ("s", Action::Archive),
        ("m", Action::Compose),
        ("r", Action::Reply),
        ("R", Action::ReplyAll),
        ("|", Action::Pipe),
        ("q", Action::Quit),
    ] {
        assert_eq!(
            map.action(key),
            Some(action),
            "`{key}` should be {action:?}"
        );
    }
}

/// The mutt-derived letters are muscle memory and stay put; only movement and
/// cancellation change.
#[test]
fn the_emacs_keymap_keeps_the_mutt_letters() {
    let map = Keymap::emacs();
    assert_eq!(map.action("C-n"), Some(Action::Down));
    assert_eq!(map.action("C-p"), Some(Action::Up));
    assert_eq!(map.action("C-g"), Some(Action::Back));
    assert_eq!(map.action("j"), None, "vim movement should be gone");

    assert_eq!(map.action("d"), Some(Action::Delete));
    assert_eq!(map.action("r"), Some(Action::Reply));
    assert_eq!(map.action("m"), Some(Action::Compose));
}

/// §12: `archive = "e"` means archive is on `e`, not that it is now on both.
/// Leaving the old key live is how a rebinding silently fails to take effect.
#[test]
fn an_override_replaces_rather_than_adds() {
    let mut map = Keymap::vim();
    assert_eq!(map.action("s"), Some(Action::Archive));

    let overrides = [("archive".to_string(), "e".to_string())]
        .into_iter()
        .collect();
    assert!(map.apply_overrides(&overrides).is_empty());

    assert_eq!(map.action("e"), Some(Action::Archive));
    assert_eq!(map.action("s"), None, "the old binding should be gone");
}

/// A typo in `[keys]` that silently does nothing is a rebinding the user
/// believes is live.
#[test]
fn unknown_bindings_are_reported() {
    let mut map = Keymap::vim();
    let overrides = [
        ("archive".to_string(), "e".to_string()),
        ("nonsense".to_string(), "z".to_string()),
    ]
    .into_iter()
    .collect();

    let unknown = map.apply_overrides(&overrides);
    assert_eq!(unknown, vec!["nonsense".to_string()]);
    assert_eq!(
        map.action("e"),
        Some(Action::Archive),
        "the good one applied"
    );
}

#[test]
fn a_config_selects_the_base_keymap() {
    let mut config = mailbox_core::config::Config::default();
    config.keys.insert("keymap".into(), "emacs".into());
    config.keys.insert("archive".into(), "e".into());

    let (map, unknown) = Keymap::from_config(&config);
    assert!(unknown.is_empty(), "`keymap` is not an action: {unknown:?}");
    assert_eq!(map.action("C-n"), Some(Action::Down), "emacs base");
    assert_eq!(map.action("e"), Some(Action::Archive), "override applied");
}

/// Action names round-trip, so `keys.toml` and the help view agree about what
/// something is called.
#[test]
fn action_names_round_trip() {
    for action in [
        Action::Down,
        Action::ToggleFlagged,
        Action::ReplyAll,
        Action::ScrollDown,
        Action::Quit,
    ] {
        let name = action_name(action);
        assert!(!name.is_empty(), "{action:?} has no name");
        assert_eq!(
            action_by_name(&name),
            Some(action),
            "{name} did not round-trip"
        );
        assert!(
            !name.contains('_'),
            "{name} should be kebab-case for a config file"
        );
    }
}

#[test]
fn every_action_has_a_key_and_a_description() {
    let map = Keymap::vim();
    for action in [
        Action::Quit,
        Action::Back,
        Action::Down,
        Action::Up,
        Action::PageDown,
        Action::PageUp,
        Action::Top,
        Action::Bottom,
        Action::Open,
        Action::Search,
        Action::Thread,
        Action::ToggleRead,
        Action::ToggleFlagged,
        Action::Tag,
        Action::Archive,
        Action::Delete,
        Action::Spam,
        Action::Reply,
        Action::ReplyAll,
        Action::Forward,
        Action::Compose,
        Action::Pipe,
        Action::Sync,
        Action::Help,
        Action::ScrollDown,
        Action::ScrollUp,
    ] {
        assert!(
            !map.keys_for(action).is_empty(),
            "{action:?} is unreachable in the default keymap"
        );
        assert!(
            !action.describe().is_empty(),
            "{action:?} has no description"
        );
    }
}

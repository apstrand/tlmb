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
        ("s", Action::ToggleRead),
        ("a", Action::Archive),
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
    assert_eq!(map.action("s"), Some(Action::ToggleRead));

    let overrides = [("archive".to_string(), "e".to_string())]
        .into_iter()
        .collect();
    assert!(map.apply_overrides(&overrides).is_empty());

    assert_eq!(map.action("e"), Some(Action::Archive));
    assert_eq!(
        map.action("a"),
        None,
        "the old archive binding should be gone"
    );
    assert_eq!(map.action("s"), Some(Action::ToggleRead));
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

// ---- following a link (§7, §9) ----

fn opened_with_links(links: &[&str]) -> App {
    use mailbox_api::{Body, BodyFormat, MessageDetail};
    let mut app = app_with(2);
    app.view = View::Message;
    app.open = Some(MessageDetail {
        summary: message("id00", "Subject 0", true),
        message_id: None,
        in_reply_to: None,
        references: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        headers: Vec::new(),
        body: Some(Body {
            format: BodyFormat::Text,
            content: "see [1] and [2]".into(),
            truncated: false,
            links: links.iter().map(|s| s.to_string()).collect(),
        }),
        attachments: Vec::new(),
    });
    app
}

#[test]
fn a_link_is_opened_only_by_picking_it() {
    let mut app = opened_with_links(&["https://one.example/a", "https://two.example/b"]);

    assert_eq!(
        app.handle(Action::Links),
        Effect::None,
        "opening the picker opens nothing"
    );
    assert_eq!(app.view, View::Links);

    app.handle(Action::Down);
    assert_eq!(
        app.handle(Action::Open),
        Effect::OpenUrl("https://two.example/b".into()),
        "Enter opens the highlighted link, and only then"
    );
}

#[test]
fn the_link_cursor_is_clamped_like_every_other() {
    let mut app = opened_with_links(&["https://one.example/a", "https://two.example/b"]);
    app.handle(Action::Links);

    app.handle(Action::Up);
    assert_eq!(app.link_selected, 0);
    for _ in 0..5 {
        app.handle(Action::Down);
    }
    assert_eq!(app.link_selected, 1);
}

/// An empty pane looks like a failure to load, so a message with no links says
/// so and stays where it was.
#[test]
fn a_message_with_no_links_says_so() {
    let mut app = opened_with_links(&[]);

    app.handle(Action::Links);
    assert_eq!(app.view, View::Message);
    assert!(app.status.contains("no links"), "{}", app.status);
}

/// The picker was opened from the message; backing out returns there rather
/// than closing the mail and losing the reader's place.
#[test]
fn leaving_the_picker_returns_to_the_message() {
    let mut app = opened_with_links(&["https://one.example/a"]);
    app.handle(Action::Links);

    app.handle(Action::Back);
    assert_eq!(app.view, View::Message);
    assert!(app.open.is_some(), "the message must still be open");
}

// ---- tagging and snoozing (§7) ----

/// The interface used to say "tagged invoices" and write nothing. A status line
/// that reports a write which did not happen is worse than no feature.
#[test]
fn tagging_asks_for_the_write_it_reports() {
    use mailbox_tui::app::Write;
    let mut app = app_with(3);

    app.handle(Action::Tag);
    for c in "invoices".chars() {
        app.type_into_prompt(c);
    }
    assert_eq!(
        app.submit_prompt(),
        Effect::Write(Write::Tag {
            id: "id00".into(),
            tag: "invoices".into()
        })
    );
}

#[test]
fn snoozing_asks_for_the_write_with_what_was_typed() {
    use mailbox_tui::app::Write;
    let mut app = app_with(3);

    app.handle(Action::Snooze);
    assert!(matches!(app.prompt, Some(Prompt::Snooze(_))));
    for c in "2h".chars() {
        app.type_into_prompt(c);
    }
    assert_eq!(
        app.submit_prompt(),
        Effect::Write(Write::Snooze {
            id: "id00".into(),
            until: "2h".into()
        }),
        "the time is passed through as text: parsing it needs the clock, which lives in the core"
    );
}

/// An empty prompt is a cancelled one. Snoozing until nothing is not a write.
#[test]
fn an_empty_snooze_prompt_writes_nothing() {
    let mut app = app_with(3);
    app.handle(Action::Snooze);
    assert_eq!(app.submit_prompt(), Effect::None);
}

// ---- the help overlay ----

/// Help is an overlay on whatever you were doing. Being dropped at the list
/// afterwards loses the message you were reading, which is the specific
/// annoyance of asking what a key does *while* using it.
#[test]
fn closing_the_help_returns_to_what_was_underneath_it() {
    for close_with in [Action::Quit, Action::Back, Action::Help] {
        let mut app = app_with(3);
        app.handle(Action::Open);
        assert_eq!(app.view, View::Message);

        app.handle(Action::Help);
        assert_eq!(app.view, View::Help);

        app.handle(close_with);
        assert_eq!(
            app.view,
            View::Message,
            "{close_with:?} left the message instead of closing the help"
        );
    }
}

#[test]
fn help_opened_from_the_list_still_closes_to_the_list() {
    let mut app = app_with(3);
    app.handle(Action::Help);
    app.handle(Action::Quit);
    assert_eq!(app.view, View::List);
}

/// Including from the link picker, which is itself reached from a message.
#[test]
fn help_returns_to_the_link_picker() {
    let mut app = opened_with_links(&["https://one.example/a"]);
    app.handle(Action::Links);

    app.handle(Action::Help);
    app.handle(Action::Help);
    assert_eq!(app.view, View::Links);
}

/// §7's mutt muscle memory: opening a message is reading it. No pause is
/// needed here — `j` and `k` move the cursor without opening anything, so
/// opening is always a deliberate act.
#[test]
fn opening_a_message_marks_it_read() {
    use mailbox_tui::app::Write;
    let mut app = app_with(3);
    // `app_with` alternates, so the second row is the unread one.
    app.handle(Action::Down);

    assert_eq!(
        app.handle(Action::Open),
        Effect::Write(Write::MarkRead { id: "id01".into() })
    );
    assert_eq!(app.view, View::Message, "and it still opens");
}

#[test]
fn opening_an_already_read_message_writes_nothing() {
    let mut app = app_with(3);
    assert_eq!(app.handle(Action::Open), Effect::None);
    assert_eq!(app.view, View::Message);
}

/// Moving the cursor in the index marks nothing: the message is not open.
#[test]
fn moving_the_cursor_marks_nothing() {
    let mut app = app_with(3);
    for _ in 0..3 {
        assert_eq!(app.handle(Action::Down), Effect::None);
        assert_eq!(app.handle(Action::Up), Effect::None);
    }
}

// ---- a listing that went stale under someone ----

/// A message opened and then found to be gone drops back to the list rather
/// than leaving the interface in a message view with nothing in it.
///
/// The sequence this comes from: the web interface deletes a message, the TUI
/// still has the row, and the next Enter finds nothing behind it.
#[test]
fn losing_the_open_message_returns_to_the_listing() {
    let mut app = app_with(2);
    app.handle(Action::Open);
    assert_eq!(app.view, View::Message);

    app.back_to_list();
    assert_eq!(app.view, View::List);
    assert!(
        app.open.is_none(),
        "the previous body must not stay on screen under a missing message"
    );
    assert_eq!(app.body_scroll, 0);
}

// ---- the panes that slide in and out (§7) ----
//
// The window keeps three panes open; a terminal is narrower, so the folders
// and the message come in when the arrows ask for them and go away again.
// What is checked here is that every one of those moves is reversible and
// that the list underneath never loses its place.

fn app_with_folders(count: usize) -> App {
    let mut app = app_with(count);
    app.folders = vec![
        mailbox_tui::app::FolderRow {
            account: "test".into(),
            path: "INBOX".into(),
            label: "Inbox".into(),
            unread: 3,
            total: 10,
        },
        mailbox_tui::app::FolderRow {
            account: "test".into(),
            path: "Some Folder".into(),
            label: "Some Folder".into(),
            unread: 0,
            total: 2,
        },
    ];
    app
}

/// The index opens on the mail, not on a list of places mail could be.
#[test]
fn the_folder_pane_starts_shut() {
    let app = app_with_folders(5);
    assert!(!app.folders_open);
    assert!(!app.preview);
    assert_eq!(app.focus, mailbox_tui::app::Focus::List);
}

/// Left opens the folders and points the keys at them; left again puts them
/// away. Nothing in between disturbs the list.
#[test]
fn left_opens_the_folder_pane_and_left_again_closes_it() {
    let mut app = app_with_folders(5);
    app.selected = 3;

    app.handle(Action::PaneLeft);
    assert!(app.folders_open);
    assert_eq!(app.focus, mailbox_tui::app::Focus::Folders);

    // Up and down now walk folders, and the message cursor stays put.
    app.handle(Action::Down);
    assert_eq!(app.folder_selected, 1);
    assert_eq!(app.selected, 3, "the message cursor moved with the folders");

    app.handle(Action::PaneLeft);
    assert!(!app.folders_open);
    assert_eq!(app.focus, mailbox_tui::app::Focus::List);
    assert_eq!(app.selected, 3);
}

/// Choosing a folder is what the pane is for, so it puts itself away.
#[test]
fn choosing_a_folder_files_the_pane_away_and_asks_for_that_folder() {
    let mut app = app_with_folders(5);
    app.handle(Action::PaneLeft);
    app.handle(Action::Down);
    app.handle(Action::Open);

    assert!(!app.folders_open, "the pane stayed open after choosing");
    assert_eq!(app.focus, mailbox_tui::app::Focus::List);
    // A path with a space has to survive into the query as one term.
    assert_eq!(app.query, r#"account:test in:"Some Folder""#);
    assert!(app.dirty, "the listing was not asked to reload");
    assert_eq!(app.selected, 0);
}

/// Right shows the message beside the list without handing it the keys, so
/// arrowing down the list keeps moving the list.
#[test]
fn right_previews_the_message_and_right_again_focuses_it() {
    let mut app = app_with_folders(5);
    app.selected = 1;

    app.handle(Action::PaneRight);
    assert!(app.preview);
    assert_eq!(
        app.focus,
        mailbox_tui::app::Focus::List,
        "the preview took the keys"
    );
    assert_eq!(
        app.view,
        View::List,
        "the list was replaced rather than shared"
    );

    // Still driving the list.
    app.handle(Action::Down);
    assert_eq!(app.selected, 2);

    app.handle(Action::PaneRight);
    assert_eq!(app.focus, mailbox_tui::app::Focus::Message);
    // Now down scrolls the message instead of moving the list.
    let before = app.selected;
    app.handle(Action::Down);
    assert_eq!(app.selected, before, "the list moved while reading");
    assert_eq!(app.body_scroll, 1);
}

/// A glance is not a read. Marking every message seen on the way past is the
/// exact thing the window's dwell exists to prevent.
#[test]
fn a_preview_does_not_mark_read_until_it_is_focused() {
    let mut app = app_with_folders(5);
    app.selected = 1; // `id01`, unread in the fixture.

    assert_eq!(app.handle(Action::PaneRight), Effect::None);
    match app.handle(Action::PaneRight) {
        Effect::Write(mailbox_tui::app::Write::MarkRead { id }) => assert_eq!(id, "id01"),
        other => panic!("focusing the preview did not mark it read: {other:?}"),
    }
}

/// Left closes the message and gives the list back, which is what makes the
/// arrows a loop rather than a one-way trip.
#[test]
fn left_closes_the_preview_and_returns_to_the_list() {
    let mut app = app_with_folders(5);
    app.handle(Action::PaneRight);
    app.handle(Action::PaneRight);
    assert_eq!(app.focus, mailbox_tui::app::Focus::Message);

    app.handle(Action::PaneLeft);
    assert!(!app.preview);
    assert_eq!(app.focus, mailbox_tui::app::Focus::List);
    assert!(app.open.is_none());
}

/// Tab is the other way across, for fingers that expect it.
#[test]
fn tab_moves_between_the_panes_that_are_open() {
    let mut app = app_with_folders(5);
    // With nothing open there is nowhere to go, and that is not an error.
    app.handle(Action::FocusNext);
    assert_eq!(app.focus, mailbox_tui::app::Focus::List);

    app.handle(Action::PaneRight);
    app.handle(Action::FocusNext);
    assert_eq!(app.focus, mailbox_tui::app::Focus::Message);
    app.handle(Action::FocusNext);
    assert_eq!(app.focus, mailbox_tui::app::Focus::List);
}

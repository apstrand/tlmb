//! The shared interface's behaviour (§7), against a backend that is not a
//! mailbox.
//!
//! §15: M5's UI work is "covered through the same CLI/JSON assertions rather
//! than a screenshot suite". This is that coverage. Everything asserted here
//! is a rule §7 states, checked without a window, a store, or a socket.

use mailbox_api::{
    Address, Body, BodyFormat, Folder, MessageDetail, MessageList, MessageSummary, ShowResult,
};
use mailbox_ui::backend::{Backend, Request, Response, Seq};
use mailbox_ui::App;

/// A backend that records what was asked and answers when told to.
///
/// Answering on command rather than immediately is the point: over HTTP the
/// answers arrive whenever they arrive, and the interface has to be right for
/// every order they might come in.
#[derive(Default)]
struct Fake {
    sent: Vec<(Seq, Request)>,
    ready: Vec<(Seq, Response)>,
}

impl Backend for Fake {
    fn send(&mut self, seq: Seq, request: Request) {
        self.sent.push((seq, request));
    }
    fn poll(&mut self) -> Option<(Seq, Response)> {
        if self.ready.is_empty() {
            None
        } else {
            Some(self.ready.remove(0))
        }
    }
    fn label(&self) -> String {
        "fake".into()
    }
}

impl Fake {
    /// The sequence number of the last request matching a predicate.
    fn seq_of(&self, matching: impl Fn(&Request) -> bool) -> Seq {
        self.sent
            .iter()
            .rev()
            .find(|(_, r)| matching(r))
            .unwrap_or_else(|| panic!("no such request in {:?}", self.sent))
            .0
    }

    fn last_list_query(&self) -> String {
        self.sent
            .iter()
            .rev()
            .find_map(|(_, r)| match r {
                Request::List { query, .. } => Some(query.clone()),
                _ => None,
            })
            .expect("a listing was requested")
    }

    fn requests(&self) -> Vec<Request> {
        self.sent.iter().map(|(_, r)| r.clone()).collect()
    }
}

fn message(id: &str, subject: &str, flags: &[&str]) -> MessageSummary {
    MessageSummary {
        id: id.into(),
        account: "test".into(),
        folder: "INBOX".into(),
        thread_id: id.into(),
        date: "2026-09-01T12:00:00Z".into(),
        from: vec![Address::new(Some("Alice".into()), "alice@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        subject: subject.into(),
        flags: flags.iter().map(|f| f.to_string()).collect(),
        tags: Vec::new(),
        has_attachment: false,
        size: 100,
        preview: Some("preview".into()),
    }
}

fn listing(ids: &[&str]) -> Response {
    Response::List(Ok(Box::new(MessageList {
        messages: ids
            .iter()
            .map(|id| message(id, &format!("Subject {id}"), &[]))
            .collect(),
        cursor: None,
        total: None,
    })))
}

fn opened(id: &str) -> Response {
    Response::Opened(Ok(Box::new(ShowResult {
        message: Some(MessageDetail {
            summary: message(id, "Subject", &[]),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            headers: Vec::new(),
            body: Some(Body {
                format: BodyFormat::Md,
                content: format!("body of {id}"),
                truncated: false,
                links: Vec::new(),
            }),
            attachments: Vec::new(),
        }),
        thread: None,
    })))
}

/// An interface with two messages listed and the first one open.
fn loaded() -> (App, Fake) {
    let mut app = App::new();
    let mut backend = Fake::default();

    app.start(&mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((seq, listing(&["id1", "id2"])));
    app.pump(&mut backend);

    let seq = backend.seq_of(|r| matches!(r, Request::Open { .. }));
    backend.ready.push((seq, opened("id1")));
    app.pump(&mut backend);

    (app, backend)
}

// ---- loading ----

#[test]
fn the_first_frame_asks_for_the_inbox_and_the_folders() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    let requests = backend.requests();
    assert!(
        requests.contains(&Request::List {
            query: String::new(),
            limit: 200
        }),
        "{requests:?}"
    );
    assert!(requests.contains(&Request::Folders), "{requests:?}");
}

/// §7: no modal waits. The interface is usable while requests are outstanding,
/// and says it is working rather than blocking.
#[test]
fn work_is_outstanding_rather_than_waited_on() {
    let mut app = App::new();
    let mut backend = Fake::default();

    app.start(&mut backend);
    // The listing, the folders, and who we may write as.
    assert_eq!(app.in_flight, 3, "the first load should be three requests");
    assert!(app.messages.is_empty(), "nothing was waited for");

    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((seq, listing(&["id1"])));
    app.pump(&mut backend);
    assert_eq!(app.messages.len(), 1);
}

/// A reading pane that fills in as the cursor moves is the difference between
/// a list and a mail client.
#[test]
fn the_first_message_opens_by_itself() {
    let (app, _) = loaded();
    assert_eq!(app.selected, Some(0));
    assert_eq!(
        app.open
            .as_ref()
            .map(|m| m.body.as_ref().unwrap().content.clone()),
        Some("body of id1".to_string())
    );
}

// ---- moving around ----

#[test]
fn the_selection_is_clamped_and_opens_what_it_lands_on() {
    let (mut app, mut backend) = loaded();

    app.move_selection(1, &mut backend);
    assert_eq!(app.selected, Some(1));
    assert!(app.open.is_none(), "the old message must not linger");
    assert!(
        matches!(backend.requests().last(), Some(Request::Open { id }) if id == "id2"),
        "{:?}",
        backend.requests()
    );

    app.move_selection(5, &mut backend);
    assert_eq!(app.selected, Some(1), "down at the bottom stops");
    app.move_selection(-5, &mut backend);
    assert_eq!(app.selected, Some(0), "up at the top stops");
}

#[test]
fn moving_in_an_empty_list_does_nothing() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.move_selection(1, &mut backend);
    assert_eq!(app.selected, None);
    assert!(backend.sent.is_empty());
}

// ---- the search field (§7: one field, the §6.2 grammar) ----

#[test]
fn the_search_field_runs_the_query_language() {
    let (mut app, mut backend) = loaded();

    app.search = "from:alice is:unread".into();
    app.submit_search(&mut backend);

    assert_eq!(backend.last_list_query(), "from:alice is:unread");
    assert_eq!(app.query, "from:alice is:unread");
}

#[test]
fn clearing_the_search_returns_to_the_default_listing() {
    let (mut app, mut backend) = loaded();
    app.search = "invoice".into();
    app.submit_search(&mut backend);

    app.clear_search(&mut backend);
    assert_eq!(backend.last_list_query(), "");
    assert!(app.search.is_empty());
}

/// Type three characters quickly and the answers can arrive in any order.
/// Painting a stale one is how a search box appears to ignore what was typed.
#[test]
fn a_late_answer_to_an_old_query_is_discarded() {
    let (mut app, mut backend) = loaded();

    app.search = "first".into();
    app.submit_search(&mut backend);
    let first = backend.seq_of(|r| matches!(r, Request::List { .. }));

    app.search = "second".into();
    app.submit_search(&mut backend);
    let second = backend.seq_of(|r| matches!(r, Request::List { .. }));
    assert_ne!(first, second);

    // The newer answer lands, then the older one arrives late.
    backend.ready.push((second, listing(&["new1", "new2"])));
    backend.ready.push((first, listing(&["stale"])));
    app.pump(&mut backend);

    let ids: Vec<&str> = app.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["new1", "new2"], "a stale listing was painted");
}

/// The same rule for the reading pane: move the cursor twice quickly and the
/// first message's body must not appear beside the second's row.
#[test]
fn a_late_message_body_is_discarded() {
    let (mut app, mut backend) = loaded();

    app.move_selection(1, &mut backend);
    let for_id2 = backend.seq_of(|r| matches!(r, Request::Open { .. }));
    app.move_selection(-1, &mut backend);
    let for_id1 = backend.seq_of(|r| matches!(r, Request::Open { .. }));

    backend.ready.push((for_id1, opened("id1")));
    backend.ready.push((for_id2, opened("id2")));
    app.pump(&mut backend);

    assert_eq!(
        app.open.as_ref().unwrap().body.as_ref().unwrap().content,
        "body of id1"
    );
}

// ---- triage (§7) ----

#[test]
fn read_and_flagged_toggle_rather_than_only_set() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((
        seq,
        Response::List(Ok(Box::new(MessageList {
            messages: vec![
                message("unread", "a", &[]),
                message("read", "b", &["seen", "flagged"]),
            ],
            cursor: None,
            total: None,
        }))),
    ));
    app.pump(&mut backend);

    app.select(0, &mut backend);
    app.toggle_read(&mut backend);
    assert!(
        matches!(backend.requests().last(), Some(Request::Flag { add, .. }) if add == &["seen"]),
        "{:?}",
        backend.requests()
    );

    app.select(1, &mut backend);
    app.toggle_read(&mut backend);
    assert!(
        matches!(backend.requests().last(), Some(Request::Flag { remove, .. }) if remove == &["seen"]),
        "{:?}",
        backend.requests()
    );

    app.toggle_flagged(&mut backend);
    assert!(
        matches!(backend.requests().last(), Some(Request::Flag { remove, .. }) if remove == &["flagged"]),
        "{:?}",
        backend.requests()
    );
}

#[test]
fn archiving_moves_the_selection_and_nothing_else() {
    let (mut app, mut backend) = loaded();
    app.move_to("archive", &mut backend);

    assert_eq!(
        backend.requests().last(),
        Some(&Request::Move {
            ids: vec!["id1".into()],
            folder: "archive".into()
        })
    );
}

#[test]
fn snoozing_passes_the_time_through_untouched() {
    let (mut app, mut backend) = loaded();
    app.snooze("tomorrow", &mut backend);

    assert_eq!(
        backend.requests().last(),
        Some(&Request::Snooze {
            ids: vec!["id1".into()],
            until: Some("tomorrow".into())
        }),
        "parsing the time belongs to the core, which owns the clock"
    );
}

#[test]
fn triage_with_nothing_selected_asks_for_nothing() {
    let mut app = App::new();
    let mut backend = Fake::default();

    app.move_to("archive", &mut backend);
    app.snooze("2h", &mut backend);
    app.toggle_read(&mut backend);
    assert!(backend.sent.is_empty(), "{:?}", backend.requests());
}

/// A write changes the store, so what is on screen is stale until it is
/// re-read. Leaving it is how an archived message stays in the inbox.
#[test]
fn a_finished_write_refreshes_the_listing() {
    let (mut app, mut backend) = loaded();
    app.move_to("archive", &mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::Move { .. }));

    let before = backend.sent.len();
    backend
        .ready
        .push((seq, Response::Changed(Ok("archived".into()))));
    app.pump(&mut backend);

    assert_eq!(app.status, "archived");
    assert!(
        backend.sent.len() > before
            && matches!(backend.sent.last(), Some((_, Request::List { .. }))),
        "{:?}",
        backend.requests()
    );
}

/// A refresh should not move the selection out from under someone mid-read.
#[test]
fn a_refresh_keeps_the_cursor_on_the_same_message() {
    let (mut app, mut backend) = loaded();
    app.move_selection(1, &mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::Open { .. }));
    backend.ready.push((seq, opened("id2")));
    app.pump(&mut backend);
    assert_eq!(app.selected_id().as_deref(), Some("id2"));

    // A reload where a new message arrived at the top.
    app.reload(&mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((seq, listing(&["id0", "id1", "id2"])));
    app.pump(&mut backend);

    assert_eq!(
        app.selected_id().as_deref(),
        Some("id2"),
        "the cursor followed the row rather than the index"
    );
}

// ---- failures ----

/// An error is shown, not swallowed and not fatal.
#[test]
fn a_failure_is_reported_and_the_interface_carries_on() {
    let (mut app, mut backend) = loaded();

    app.search = "nope:x".into();
    app.submit_search(&mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend
        .ready
        .push((seq, Response::List(Err("unknown key `nope`".into()))));
    app.pump(&mut backend);

    assert_eq!(app.error.as_deref(), Some("unknown key `nope`"));
    assert_eq!(app.messages.len(), 2, "the old listing is still readable");

    // And it clears when something works.
    app.clear_search(&mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((seq, listing(&["id1"])));
    app.pump(&mut backend);
    assert_eq!(app.error, None);
}

#[test]
fn folders_arrive_without_blocking_anything_else() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    let seq = backend.seq_of(|r| matches!(r, Request::Folders));
    backend.ready.push((
        seq,
        Response::Folders(Ok(Box::new(mailbox_api::FolderList {
            folders: vec![Folder {
                account: "test".into(),
                path: "INBOX".into(),
                name: "INBOX".into(),
                special_use: Some("inbox".into()),
                total: 2,
                unread: 1,
                selectable: true,
            }],
        }))),
    ));
    app.pump(&mut backend);

    assert_eq!(app.folders.len(), 1);
    assert_eq!(app.folders[0].path, "INBOX");
}

// ---- writing (§7) ----

fn identity_list() -> Response {
    Response::Identities(Ok(Box::new(mailbox_api::IdentityList {
        identities: vec![
            mailbox_api::Identity {
                account: "test".into(),
                display: "peter@example.com".into(),
                address: "peter@example.com".into(),
                primary: true,
            },
            mailbox_api::Identity {
                account: "test".into(),
                display: "p.strand@example.com".into(),
                address: "p.strand@example.com".into(),
                primary: false,
            },
        ],
    })))
}

#[test]
fn the_first_frame_asks_who_we_can_write_as() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    assert!(backend.requests().contains(&Request::Identities));

    let seq = backend.seq_of(|r| matches!(r, Request::Identities));
    backend.ready.push((seq, identity_list()));
    app.pump(&mut backend);
    assert_eq!(app.identities.len(), 2);
}

/// A blank compose window opens without asking anyone anything.
#[test]
fn composing_needs_no_round_trip() {
    let (mut app, mut backend) = loaded();
    let seq = backend.seq_of(|r| matches!(r, Request::Identities));
    backend.ready.push((seq, identity_list()));
    app.pump(&mut backend);

    let before = backend.sent.len();
    app.compose();

    assert!(app.draft.is_some());
    assert_eq!(
        app.draft.as_ref().unwrap().from.as_deref(),
        Some("peter@example.com"),
        "the From selector should start on the account's own address"
    );
    assert_eq!(
        backend.sent.len(),
        before,
        "opening a window asked the server something"
    );
}

/// Replying does need one: what a reply quotes and which addresses a reply-all
/// keeps are the core's to decide, and an interface deriving them separately is
/// how "reply" starts meaning different things in different windows.
#[test]
fn replying_asks_the_core_for_the_draft() {
    let (mut app, mut backend) = loaded();

    app.reply(mailbox_ui::backend::ReplyKind::All, &mut backend);
    assert_eq!(
        backend.requests().last(),
        Some(&Request::ReplyDraft {
            id: "id1".into(),
            mode: mailbox_ui::backend::ReplyKind::All
        })
    );
    assert!(
        app.draft.is_none(),
        "the window opens when the draft arrives"
    );

    let seq = backend.seq_of(|r| matches!(r, Request::ReplyDraft { .. }));
    backend.ready.push((
        seq,
        Response::Draft(Ok(Box::new(mailbox_api::DraftSpec {
            to: vec!["Alice <alice@example.com>".into()],
            subject: "Re: Subject".into(),
            body: "\n\n> quoted".into(),
            in_reply_to: Some("<one@example.com>".into()),
            ..Default::default()
        }))),
    ));
    app.pump(&mut backend);

    let draft = app.draft.as_ref().expect("a draft");
    assert_eq!(draft.subject, "Re: Subject");
    assert_eq!(draft.in_reply_to.as_deref(), Some("<one@example.com>"));
}

#[test]
fn replying_with_nothing_selected_asks_for_nothing() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.reply(mailbox_ui::backend::ReplyKind::Sender, &mut backend);
    assert!(backend.sent.is_empty());
}

/// §5.5: queued, not sent. Saying "sent" while the outbox still holds it is a
/// lie the moment submission fails.
#[test]
fn sending_reports_the_message_as_queued() {
    let (mut app, mut backend) = loaded();
    app.compose();
    app.draft.as_mut().unwrap().to = vec!["bob@example.com".into()];

    app.send(&mut backend);
    assert!(app.sending, "the window should say it is working");
    assert!(matches!(backend.requests().last(), Some(Request::Send(_))));

    let seq = backend.seq_of(|r| matches!(r, Request::Send(_)));
    backend.ready.push((
        seq,
        Response::Sent(Ok(Box::new(mailbox_api::SendResult {
            id: 1,
            message_id: "abc".into(),
            status: "queued".into(),
            recipients: vec!["bob@example.com".into()],
            bytes: 400,
            send_after: None,
            duplicate: false,
            raw: None,
        }))),
    ));
    app.pump(&mut backend);

    assert!(app.status.contains("queued"), "{}", app.status);
    assert!(app.draft.is_none(), "the window should close on success");
    assert!(!app.sending);
}

/// A refused send keeps the window open, because the text in it is the only
/// copy of what someone wrote.
#[test]
fn a_refused_send_does_not_throw_the_message_away() {
    let (mut app, mut backend) = loaded();
    app.compose();
    app.draft.as_mut().unwrap().to = vec!["bob@example.com".into()];
    app.draft.as_mut().unwrap().body = "Please do not lose this.".into();
    app.send(&mut backend);

    let seq = backend.seq_of(|r| matches!(r, Request::Send(_)));
    backend
        .ready
        .push((seq, Response::Sent(Err("the server said no".into()))));
    app.pump(&mut backend);

    assert_eq!(app.error.as_deref(), Some("the server said no"));
    assert_eq!(
        app.draft.as_ref().map(|d| d.body.as_str()),
        Some("Please do not lose this."),
    );
    assert!(!app.sending, "and it can be tried again");
}

/// A message with nobody to send it to is caught before it costs a round trip.
#[test]
fn a_message_with_no_recipient_is_not_sent() {
    let (mut app, mut backend) = loaded();
    app.compose();
    app.draft.as_mut().unwrap().subject = "Hello".into();

    let before = backend.sent.len();
    app.send(&mut backend);

    assert_eq!(backend.sent.len(), before);
    assert!(app
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("recipient"));
    assert!(app.draft.is_some());
}

// ---- marking read (§7) ----

/// The reading pane fills in as the cursor moves, so a pause is what stops
/// holding `j` down from marking a whole mailbox read on the way past.
#[test]
fn a_message_is_marked_read_only_after_it_has_been_open_a_while() {
    use mailbox_ui::app::READ_DWELL;
    let (mut app, mut backend) = loaded();

    let start = std::time::Instant::now();
    let before = backend.sent.len();

    app.tick(start, &mut backend);
    assert_eq!(backend.sent.len(), before, "marked read on the first frame");

    app.tick(start + READ_DWELL / 2, &mut backend);
    assert_eq!(backend.sent.len(), before, "marked read halfway through");

    app.tick(start + READ_DWELL, &mut backend);
    assert!(
        matches!(
            backend.requests().last(),
            Some(Request::Flag { ids, add, .. }) if ids == &["id1".to_string()] && add == &["seen"]
        ),
        "{:?}",
        backend.requests()
    );
}

/// Moving on before the pause elapses marks nothing, which is the whole point.
#[test]
fn cursoring_past_a_message_does_not_mark_it() {
    use mailbox_ui::app::READ_DWELL;
    let (mut app, mut backend) = loaded();

    let start = std::time::Instant::now();
    app.tick(start, &mut backend);

    // Down to the next message before the dwell elapses.
    app.move_selection(1, &mut backend);
    let seq = backend.seq_of(|r| matches!(r, Request::Open { .. }));
    backend.ready.push((seq, opened("id2")));
    app.pump(&mut backend);

    let before = backend.sent.len();
    app.tick(start + READ_DWELL, &mut backend);
    assert_eq!(
        backend.sent.len(),
        before,
        "the clock restarts with the message: {:?}",
        backend.requests()
    );

    // And the new one is marked on its own schedule.
    app.tick(start + READ_DWELL * 2, &mut backend);
    assert!(
        matches!(
            backend.requests().last(),
            Some(Request::Flag { ids, .. }) if ids == &["id2".to_string()]
        ),
        "{:?}",
        backend.requests()
    );
}

/// Once. The listing has not come back yet, so the flags this reads from still
/// say unread — without the latch it would fire every frame until it did.
#[test]
fn a_message_is_marked_read_once() {
    use mailbox_ui::app::READ_DWELL;
    let (mut app, mut backend) = loaded();

    let start = std::time::Instant::now();
    app.tick(start, &mut backend);
    app.tick(start + READ_DWELL, &mut backend);
    let after_first = backend.sent.len();

    for extra in 1..5 {
        app.tick(start + READ_DWELL * (1 + extra), &mut backend);
    }
    assert_eq!(backend.sent.len(), after_first);
}

/// A message that was already read is not written to again. A pointless write
/// is a pointless `STORE` on the server and a pointless entry in the queue.
#[test]
fn an_already_read_message_is_left_alone() {
    use mailbox_ui::app::READ_DWELL;
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((
        seq,
        Response::List(Ok(Box::new(MessageList {
            messages: vec![message("read", "already read", &["seen"])],
            cursor: None,
            total: None,
        }))),
    ));
    app.pump(&mut backend);

    let seq = backend.seq_of(|r| matches!(r, Request::Open { .. }));
    backend.ready.push((
        seq,
        Response::Opened(Ok(Box::new(ShowResult {
            message: Some(MessageDetail {
                summary: message("read", "already read", &["seen"]),
                message_id: None,
                in_reply_to: None,
                references: Vec::new(),
                reply_to: Vec::new(),
                bcc: Vec::new(),
                headers: Vec::new(),
                body: None,
                attachments: Vec::new(),
            }),
            thread: None,
        }))),
    ));
    app.pump(&mut backend);

    let start = std::time::Instant::now();
    let before = backend.sent.len();
    app.tick(start, &mut backend);
    app.tick(start + READ_DWELL * 3, &mut backend);
    assert_eq!(backend.sent.len(), before, "{:?}", backend.requests());
}

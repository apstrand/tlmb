//! What the interface actually draws, without a window.
//!
//! §15 rules out a screenshot suite, and this is the better thing anyway: egui
//! will run a frame against a bare `Context`, and the tessellated output still
//! carries the text of every label. So "the subject is on screen" can be
//! asserted directly, deterministically, and in milliseconds — and a panic in
//! the drawing code is caught by the same run.

use mailbox_api::{
    Address, Body, BodyFormat, MessageDetail, MessageList, MessageSummary, ShowResult,
};
use mailbox_ui::backend::{Backend, Request, Response, Seq};
use mailbox_ui::App;

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
        "local store — test".into()
    }
}

impl Fake {
    fn seq_of(&self, matching: impl Fn(&Request) -> bool) -> Seq {
        self.sent
            .iter()
            .rev()
            .find(|(_, r)| matching(r))
            .expect("no such request")
            .0
    }
}

fn message(id: &str, subject: &str, from: &str, flags: &[&str]) -> MessageSummary {
    MessageSummary {
        id: id.into(),
        account: "test".into(),
        folder: "INBOX".into(),
        thread_id: id.into(),
        date: "2026-08-30T09:00:00Z".into(),
        from: vec![Address::new(Some(from.into()), "someone@example.com")],
        to: Vec::new(),
        cc: Vec::new(),
        subject: subject.into(),
        flags: flags.iter().map(|f| f.to_string()).collect(),
        tags: Vec::new(),
        has_attachment: false,
        size: 100,
        preview: Some("a preview line".into()),
    }
}

/// Runs one frame and returns every piece of text that was drawn.
///
/// The galley inside a text shape holds the string it laid out, so this is
/// what a reader would see, not what the code intended to show.
fn drawn(app: &mut App, backend: &mut Fake) -> Vec<String> {
    use egui::epaint::Shape;

    let ctx = egui::Context::default();
    // A window large enough that nothing is elided for want of room.
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(1400.0, 900.0),
        )),
        ..Default::default()
    };

    let output = ctx.run(input, |ctx| {
        mailbox_ui::view::draw(ctx, app, backend);
    });

    fn collect(shape: &Shape, out: &mut Vec<String>) {
        match shape {
            Shape::Text(text) => out.push(text.galley.text().to_string()),
            Shape::Vec(shapes) => {
                for s in shapes {
                    collect(s, out);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, &mut out);
    }
    out
}

fn loaded() -> (App, Fake) {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((
        seq,
        Response::List(Ok(Box::new(MessageList {
            messages: vec![
                message("id1", "Your order has shipped", "Example Shop", &[]),
                message("id2", "Re: the parser", "Alice", &["seen"]),
            ],
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
                summary: message("id1", "Your order has shipped", "Example Shop", &[]),
                message_id: None,
                in_reply_to: None,
                references: Vec::new(),
                reply_to: Vec::new(),
                bcc: Vec::new(),
                headers: Vec::new(),
                body: Some(Body {
                    format: BodyFormat::Text,
                    // As the text rendering actually arrives: the prose, then
                    // the numbered destinations (§6.4).
                    content: "Your parcel is on its way [1].\n> and a quoted line\n\n\
                              Links:\n[1] https://track.example/9f3a"
                        .into(),
                    truncated: false,
                    links: vec!["https://track.example/9f3a".into()],
                }),
                attachments: Vec::new(),
            }),
            thread: None,
        }))),
    ));
    app.pump(&mut backend);

    (app, backend)
}

#[test]
fn the_listing_shows_sender_subject_and_preview() {
    let (mut app, mut backend) = loaded();
    let text = drawn(&mut app, &mut backend).join("\n");

    assert!(text.contains("Example Shop"), "{text}");
    assert!(text.contains("Your order has shipped"), "{text}");
    assert!(text.contains("a preview line"), "{text}");
    assert!(text.contains("Alice"), "{text}");
    // §7's date column, in a form that fits.
    assert!(text.contains("Aug 30"), "{text}");
}

#[test]
fn the_reading_pane_shows_the_open_message_and_its_links() {
    let (mut app, mut backend) = loaded();
    let text = drawn(&mut app, &mut backend).join("\n");

    assert!(text.contains("Your parcel is on its way [1]."), "{text}");
    assert!(text.contains("> and a quoted line"), "{text}");
    // §9: the destination is shown, and nothing was fetched to draw it.
    assert!(text.contains("https://track.example/9f3a"), "{text}");
    // Once. The body's own list is made clickable rather than repeated
    // underneath as a second one.
    assert_eq!(
        text.matches("https://track.example/9f3a").count(),
        1,
        "the links were listed twice: {text}"
    );
}

/// §7: one search field, and it says what it takes.
#[test]
fn the_search_field_advertises_the_query_grammar() {
    let (mut app, mut backend) = loaded();
    let text = drawn(&mut app, &mut backend).join("\n");

    assert!(text.contains("from:alice"), "{text}");
    assert!(text.contains("is:unread"), "{text}");
}

/// The triage §7 names, reachable without knowing a keystroke.
#[test]
fn triage_is_on_screen() {
    let (mut app, mut backend) = loaded();
    let text = drawn(&mut app, &mut backend).join("\n");

    for button in ["Archive", "Delete", "Spam", "Tomorrow", "Sync"] {
        assert!(text.contains(button), "no {button}: {text}");
    }
}

#[test]
fn where_the_mail_is_coming_from_is_visible() {
    let (mut app, mut backend) = loaded();
    let text = drawn(&mut app, &mut backend).join("\n");
    assert!(text.contains("local store — test"), "{text}");
}

/// An empty mailbox, a failed request, and a message that has not arrived yet
/// all have to draw something rather than nothing.
#[test]
fn the_empty_and_broken_states_say_what_is_happening() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    let text = drawn(&mut app, &mut backend).join("\n");
    assert!(text.contains("no messages"), "{text}");

    app.error = Some("unknown key `nope`".into());
    let text = drawn(&mut app, &mut backend).join("\n");
    assert!(text.contains("unknown key `nope`"), "{text}");
}

/// Drawing must not panic on a message with nothing in it — no sender, no
/// subject, no body — which is what a half-synced mailbox contains.
#[test]
fn a_message_with_nothing_in_it_still_draws() {
    let mut app = App::new();
    let mut backend = Fake::default();
    app.start(&mut backend);

    let mut empty = message("", "", "", &[]);
    empty.from.clear();
    empty.preview = None;
    let seq = backend.seq_of(|r| matches!(r, Request::List { .. }));
    backend.ready.push((
        seq,
        Response::List(Ok(Box::new(MessageList {
            messages: vec![empty.clone()],
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
                summary: empty,
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

    let text = drawn(&mut app, &mut backend).join("\n");
    assert!(text.contains("(unknown sender)"), "{text}");
    assert!(text.contains("[body not downloaded]"), "{text}");
}

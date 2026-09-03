//! Wrapping a body, and the terminal hyperlinks that ride on knowing where
//! each label landed (§7).

use mailbox_tui::markdown::{body_lines, wrap, Body, BodyLine, LinkSpan, Piece};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

/// The visible text of each wrapped line.
fn wrapped(md: &str, width: usize) -> Vec<String> {
    wrap(&body_lines(md, width, &[]), width)
        .iter()
        .map(|line| {
            line.iter()
                .map(|p| p.text.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn text_wraps_on_word_boundaries() {
    assert_eq!(
        wrapped("the quick brown fox jumps over the lazy dog", 20),
        vec!["the quick brown fox", "jumps over the lazy", "dog"]
    );
}

/// The reason wrapping is done here rather than by `Paragraph`: a quote whose
/// second visual line has no `>` reads as the reply rather than the quotation.
#[test]
fn a_wrapped_quote_keeps_its_marker_on_every_line() {
    let lines = wrapped("> the quick brown fox jumps over the lazy dog", 20);

    assert!(lines.len() > 1, "{lines:?}");
    for line in &lines {
        assert!(line.starts_with("> "), "{line:?} lost the quote marker");
    }
}

#[test]
fn a_nested_quote_keeps_both_markers() {
    for line in wrapped("> > deeply quoted text that will not fit on one line", 20) {
        assert!(line.starts_with("> > "), "{line:?}");
    }
}

/// A wrapped list item hangs under its text, not under its bullet, so it still
/// reads as one item.
#[test]
fn a_wrapped_list_item_hangs_under_its_text() {
    let lines = wrapped("- the quick brown fox jumps over the lazy dog", 20);

    assert_eq!(lines[0], "• the quick brown");
    assert!(lines[1].starts_with("  "), "{:?}", lines[1]);
    assert!(
        !lines[1].starts_with("• "),
        "the bullet repeated: {:?}",
        lines[1]
    );
}

/// A URL written out in prose is one word and can be longer than the pane. It
/// gets broken rather than allowed to run off the edge and vanish.
#[test]
fn a_word_too_long_for_the_line_is_broken() {
    let long = "https://example.com/a/very/long/path/that/will/not/fit/anywhere";
    let lines = wrapped(long, 20);

    assert!(lines.len() > 2, "{lines:?}");
    for line in &lines {
        assert!(line.width_chars() <= 20, "{line:?} is wider than the pane");
    }
    assert_eq!(lines.concat(), long, "the URL was altered, not just broken");
}

trait WidthChars {
    fn width_chars(&self) -> usize;
}
impl WidthChars for String {
    fn width_chars(&self) -> usize {
        self.chars().count()
    }
}

#[test]
fn blank_lines_survive_wrapping() {
    assert_eq!(wrapped("one\n\ntwo", 20), vec!["one", "", "two"]);
}

// ---- terminal hyperlinks ----

/// Renders and reports where the links landed.
fn spans(lines: &[BodyLine], links: &[String], width: u16) -> Vec<LinkSpan> {
    let area = Rect::new(0, 0, width, 6);
    let mut buf = Buffer::empty(area);
    let mut found = Vec::new();
    Body {
        lines,
        links,
        scroll: 0,
        found: Some(&mut found),
    }
    .render(area, &mut buf);
    found
}

fn linked_line(text: &str) -> Vec<BodyLine> {
    vec![BodyLine {
        lead: Piece::new("see ", Style::default()),
        hang: Piece::new("", Style::default()),
        pieces: vec![Piece {
            text: text.to_string(),
            style: Style::default().fg(mailbox_tui::theme::LINK),
            link: Some(0),
        }],
    }]
}

/// The positions come from the same layout pass that drew the cells, because a
/// second pass that recomputed them could disagree — and a hyperlink one column
/// out is a hyperlink over the wrong text.
#[test]
fn a_links_label_is_reported_where_it_was_drawn() {
    let links = vec!["https://example.com/x".to_string()];
    let found = spans(&linked_line("here"), &links, 20);

    assert_eq!(found.len(), 1);
    assert_eq!(found[0].x, 4, "after `see `");
    assert_eq!(found[0].y, 0);
    assert_eq!(found[0].text, "here");
    assert_eq!(found[0].url, "https://example.com/x");
}

/// A label clipped by the pane's edge is reported as what was drawn, not as
/// what it wanted to be: the reprint has to match the cells exactly.
#[test]
fn a_clipped_label_is_reported_clipped() {
    let links = vec!["https://example.com/x".to_string()];
    let found = spans(&linked_line("hereisaverylonglabel"), &links, 12);

    assert_eq!(found[0].text.chars().count(), 8, "{:?}", found[0].text);
    assert_eq!(found[0].text, "hereisav");
}

/// A label split by wrapping is two runs, and both need marking up.
#[test]
fn a_link_split_across_lines_is_reported_twice() {
    let links = vec!["https://example.com/x".to_string()];
    let lines = vec![BodyLine {
        lead: Piece::new("", Style::default()),
        hang: Piece::new("", Style::default()),
        pieces: vec![Piece {
            text: "one two three four".into(),
            style: Style::default(),
            link: Some(0),
        }],
    }];
    let found = spans(&lines, &links, 10);

    assert!(found.len() >= 2, "{found:?}");
    assert!(found.iter().all(|s| s.url == "https://example.com/x"));
    assert!(
        found[0].y != found[1].y,
        "the two halves should be on different rows: {found:?}"
    );
}

/// A run with no link, or a link index the body's list does not carry, is not
/// reported — a hyperlink to nothing is worse than none.
#[test]
fn only_real_links_are_reported() {
    assert!(spans(&linked_line("here"), &[], 20).is_empty());

    let plain = vec![BodyLine::plain(vec![Piece::new(
        "no link",
        Style::default(),
    )])];
    assert!(spans(&plain, &["https://x/".to_string()], 20).is_empty());
}

/// The label is reprinted under the hyperlink, so it has to carry the style it
/// was drawn with or it would change colour when it became clickable.
#[test]
fn the_reported_style_is_the_one_it_was_drawn_with() {
    let links = vec!["https://example.com/x".to_string()];
    let found = spans(&linked_line("here"), &links, 20);

    assert_eq!(found[0].style.fg, Some(mailbox_tui::theme::LINK));
    assert_eq!(
        mailbox_tui::markdown::sgr(found[0].style),
        "\x1b[0;34m",
        "the escape that restores it"
    );
}

#[test]
fn the_style_escape_covers_the_attributes_in_use() {
    use ratatui::style::Modifier;

    assert_eq!(mailbox_tui::markdown::sgr(Style::default()), "\x1b[0m");
    assert_eq!(
        mailbox_tui::markdown::sgr(
            Style::default()
                .fg(mailbox_tui::theme::DIM)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        ),
        "\x1b[0;90;1;4m"
    );
    assert_eq!(
        mailbox_tui::markdown::sgr(Style::default().add_modifier(Modifier::REVERSED)),
        "\x1b[0;7m"
    );
}

/// `MAILBOX_HYPERLINKS=0` for terminals that would show the escape instead of
/// acting on it.
#[test]
fn hyperlinks_can_be_turned_off() {
    std::env::set_var("MAILBOX_HYPERLINKS", "0");
    let off = mailbox_tui::markdown::hyperlinks_wanted();
    std::env::set_var("MAILBOX_HYPERLINKS", "1");
    let on = mailbox_tui::markdown::hyperlinks_wanted();
    std::env::remove_var("MAILBOX_HYPERLINKS");

    assert!(!off);
    assert!(on);
}

/// The bytes that actually reach the terminal: position, hyperlink, style,
/// label, close.
#[test]
fn the_hyperlink_escape_is_written_at_the_labels_position() {
    let spans = vec![LinkSpan {
        x: 4,
        y: 9,
        text: "track it here".into(),
        style: Style::default().fg(mailbox_tui::theme::LINK),
        url: "https://track.example/p/9f3a".into(),
    }];
    let mut out: Vec<u8> = Vec::new();
    mailbox_tui::markdown::write_hyperlinks(&mut out, &spans).unwrap();

    assert_eq!(
        String::from_utf8(out).unwrap(),
        // Cursor to row 10, column 5 — the escape is 1-based, the buffer is 0.
        "\x1b[10;5H\
         \x1b]8;id=mailbox0;https://track.example/p/9f3a\x1b\\\
         \x1b[0;34mtrack it here\
         \x1b]8;;\x1b\\\x1b[0m"
    );
}

/// A URL that could close the sequence early would put the rest of itself on
/// screen as text, over the message.
#[test]
fn a_url_that_could_break_out_is_not_written() {
    for hostile in ["https://x/\x1b]8;;\x1b\\", "https://x/\x07", ""] {
        let spans = vec![LinkSpan {
            x: 0,
            y: 0,
            text: "label".into(),
            style: Style::default(),
            url: hostile.into(),
        }];
        let mut out: Vec<u8> = Vec::new();
        mailbox_tui::markdown::write_hyperlinks(&mut out, &spans).unwrap();
        assert!(out.is_empty(), "{hostile:?} was written: {out:?}");
    }
}

/// Whitespace at a line break disappears with the break; whitespace at the
/// start of a line is indentation. Without the distinction, an indented code
/// line slides left by however many spaces it began with.
#[test]
fn leading_indentation_is_not_mistaken_for_a_break() {
    let lines = vec![BodyLine::plain(vec![Piece::new(
        "    let n = 2;",
        Style::default(),
    )])];
    let visual = wrap(&lines, 40);
    let text: String = visual[0].iter().map(|p| p.text.as_str()).collect();

    assert_eq!(text, "    let n = 2;");
}

#[test]
fn a_run_of_spaces_inside_a_line_is_kept() {
    let lines = vec![BodyLine::plain(vec![Piece::new(
        "let n = 2;   /* 24 */",
        Style::default(),
    )])];
    let visual = wrap(&lines, 40);
    let text: String = visual[0].iter().map(|p| p.text.as_str()).collect();

    assert_eq!(text, "let n = 2;   /* 24 */");
}

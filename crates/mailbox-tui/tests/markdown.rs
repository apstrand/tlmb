//! The styled body renderer (§7).
//!
//! Two things are being held down here. Markdown a message actually contains
//! turns into styles rather than being shown as punctuation; and plain-text
//! mail, which comes through the same path and was never Markdown, is not
//! damaged on the way.

use mailbox_tui::markdown::body_lines;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

/// The visible text of a rendered line, markers and all removed.
fn text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn render(md: &str) -> Vec<Line<'static>> {
    body_lines(md, 40, &[])
}

fn one(md: &str) -> Line<'static> {
    render(md).into_iter().next().expect("a line")
}

/// Finds the span carrying `needle` and returns its style.
fn style_of(line: &Line<'_>, needle: &str) -> Style {
    line.spans
        .iter()
        .find(|s| s.content.contains(needle))
        .unwrap_or_else(|| panic!("no span containing {needle:?} in {:?}", text(line)))
        .style
}

// ---- markers become styles ----

#[test]
fn emphasis_is_styled_not_shown() {
    let line = one("a **bold** and *italic* and `code` word");

    assert_eq!(text(&line), "a bold and italic and code word");
    assert!(style_of(&line, "bold")
        .add_modifier
        .contains(Modifier::BOLD));
    assert!(style_of(&line, "italic")
        .add_modifier
        .contains(Modifier::ITALIC));
    assert_eq!(style_of(&line, "code").fg, Some(mailbox_tui::theme::CODE));
}

#[test]
fn a_heading_loses_its_hashes_and_gains_weight() {
    let line = one("## Your order has shipped");

    assert_eq!(text(&line), "Your order has shipped");
    assert!(style_of(&line, "order")
        .add_modifier
        .contains(Modifier::BOLD));
}

/// `#4471` in a subject is not a heading, and a message about an issue number
/// should not have half of it rendered as one.
#[test]
fn a_hash_without_a_space_is_not_a_heading() {
    assert_eq!(text(&one("#4471 has shipped")), "#4471 has shipped");
}

#[test]
fn list_items_get_a_bullet() {
    let lines = render("- one\n- two\n1. first\n2. second");

    assert_eq!(text(&lines[0]), "• one");
    assert_eq!(text(&lines[2]), "1. first");
    assert_eq!(text(&lines[3]), "2. second");
}

#[test]
fn a_quote_is_dimmed_including_its_marker() {
    let line = one("> > what they wrote");

    assert_eq!(text(&line), "> > what they wrote");
    assert_eq!(style_of(&line, "what").fg, Some(mailbox_tui::theme::DIM));
    assert_eq!(style_of(&line, ">").fg, Some(mailbox_tui::theme::DIM));
}

/// Emphasis inside a quote must not undo the dimming: the point of dimming a
/// quote is to separate this message from the one it answers.
#[test]
fn markup_inside_a_quote_stays_dim() {
    let line = one("> a **bold** claim");

    assert_eq!(text(&line), "> a bold claim");
    assert_eq!(style_of(&line, "bold").fg, Some(mailbox_tui::theme::DIM));
}

#[test]
fn a_fenced_block_is_shown_without_its_fences() {
    let lines = render("before\n```\nlet x = 1;\n```\nafter");
    let rendered: Vec<String> = lines.iter().map(text).collect();

    assert_eq!(rendered, vec!["before", "let x = 1;", "after"]);
    assert_eq!(
        style_of(&lines[1], "let").fg,
        Some(mailbox_tui::theme::CODE)
    );
}

/// Inside a fence, punctuation is code and not markup.
#[test]
fn a_fenced_block_is_not_parsed_for_markers() {
    let lines = render("```\nlet a = b * c * d;\n```");
    assert_eq!(text(&lines[0]), "let a = b * c * d;");
}

#[test]
fn a_rule_is_drawn_as_one() {
    let line = body_lines("---", 10, &[]).remove(0);
    assert_eq!(text(&line), "──────────");
}

// ---- links ----

#[test]
fn a_link_shows_its_label_and_the_number_the_picker_uses() {
    let links = vec![
        "https://one.example/a".to_string(),
        "https://track.example/b".to_string(),
    ];
    let line = body_lines(
        "You can [track it here](https://track.example/b) today",
        40,
        &links,
    )
    .remove(0);

    assert_eq!(text(&line), "You can track it here [2] today");
    let label = style_of(&line, "track it here");
    assert_eq!(label.fg, Some(mailbox_tui::theme::LINK));
    assert!(label.add_modifier.contains(Modifier::UNDERLINED));
}

/// The number is looked up rather than counted, so a link the body's own list
/// does not carry is shown without one instead of with a wrong one.
#[test]
fn an_unlisted_link_is_shown_without_a_number() {
    let line = body_lines("[x](https://absent.example/) here", 40, &[]).remove(0);
    assert_eq!(text(&line), "x here");
}

#[test]
fn an_image_says_it_was_an_image() {
    let line = one("![Our logo] and text");
    assert_eq!(text(&line), "[image: Our logo] and text");
}

// ---- plain text is not damaged ----

/// Plain-text mail arrives through the same path and was never Markdown. A
/// marker whose other half is missing is punctuation.
#[test]
fn an_unmatched_marker_is_left_alone() {
    for input in [
        "a * b",
        "5 stars ***",
        "an unclosed **bold",
        "a `backtick",
        "[not a link] here",
        "[label](unclosed",
    ] {
        assert_eq!(text(&one(input)), input, "{input} was mangled");
    }
}

/// The rule that makes this safe: emphasis may not begin or end against a
/// space, so arithmetic survives.
#[test]
fn arithmetic_is_not_emphasis() {
    assert_eq!(text(&one("2 * 3 * 4 = 24")), "2 * 3 * 4 = 24");
    assert_eq!(text(&one("a * b + c * d")), "a * b + c * d");
}

/// Underscores inside identifiers are not emphasis, which is the difference
/// between reading a patch in your mail and reading half of one.
#[test]
fn identifiers_keep_their_underscores() {
    for input in [
        "call parse_until_now(x)",
        "MAILBOX_STATE_DIR and MAILBOX_CONFIG",
        "snake_case_name",
    ] {
        assert_eq!(text(&one(input)), input, "{input} was mangled");
    }
    // But a genuinely emphasised word still works.
    let line = one("this is _important_ today");
    assert_eq!(text(&line), "this is important today");
    assert!(style_of(&line, "important")
        .add_modifier
        .contains(Modifier::ITALIC));
}

#[test]
fn an_empty_body_renders_nothing_rather_than_panicking() {
    assert!(render("").is_empty());
    assert_eq!(text(&one("\n")), "");
}

/// Wide characters, combining marks, and right-to-left text all go through the
/// same char-indexed scanner.
#[test]
fn non_ascii_bodies_survive() {
    for input in [
        "**世界** です",
        "e\u{301}quipe *ready*",
        "مرحبا **بالعالم**",
        "🎉 **party** 🎉",
    ] {
        let _ = one(input);
    }
    assert_eq!(text(&one("**世界** です")), "世界 です");
}

/// A newsletter's logo is a linked image — `[![alt]](url)` — and reading the
/// label as far as the first `]` leaves the URL on screen as text. Found by
/// looking at a rendered message rather than by a test, which is why there is
/// now a test.
#[test]
fn a_linked_image_shows_its_alt_text_and_nothing_else() {
    let links = vec!["https://shop.example/?utm=news".to_string()];
    let line = body_lines(
        "[![Example Shop]](https://shop.example/?utm=news)",
        40,
        &links,
    )
    .remove(0);

    assert_eq!(text(&line), "[image: Example Shop] [1]");
}

#[test]
fn markup_inside_a_link_label_is_read_too() {
    let links = vec!["https://x.example/".to_string()];
    let line = body_lines("[a **bold** label](https://x.example/)", 40, &links).remove(0);

    assert_eq!(text(&line), "a bold label [1]");
    assert!(style_of(&line, "bold")
        .add_modifier
        .contains(Modifier::BOLD));
}

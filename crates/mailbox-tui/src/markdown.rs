//! Drawing a message body as styled Markdown (§7).
//!
//! The bodies arrive as Markdown because that is what `mailbox-core` makes of
//! HTML mail, and most mail is HTML now. Showing that Markdown literally — a
//! screen of `**` and `[text](https://…)` — would be showing the reader the
//! scaffolding instead of the message, so the markers become the styles they
//! stand for and the URLs move into the link picker (`o`).
//!
//! Two rules keep this from damaging plain-text mail, which arrives through the
//! same path and was never Markdown:
//!
//! - A marker is only a marker when its closing half is on the same line. An
//!   unmatched `*` is an asterisk, and stays one.
//! - Emphasis may not begin or end against a space, so `2 * 3 * 4` is
//!   arithmetic rather than an italic `3`.
//!
//! Like [`crate::app`], this is free of any terminal: it turns text into
//! [`Line`]s, and a test can read the spans back.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme;

/// Renders a Markdown body into styled lines.
///
/// `links` is the body's own link list, and a link's position in it is the
/// number shown beside it — so what the reader sees next to a link is exactly
/// what `o` will offer them.
pub fn body_lines(md: &str, width: usize, links: &[String]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_code = false;

    for raw in md.lines() {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            // The fence is scaffolding. Drawing it would spend a row of a
            // small screen saying "code follows", which the colour says.
            continue;
        }
        if in_code {
            out.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme::CODE),
            )));
            continue;
        }
        out.push(block(raw, width, links));
    }
    out
}

/// One line, after its block-level markers have been read.
fn block(raw: &str, width: usize, links: &[String]) -> Line<'static> {
    // Quoting first: everything inside a quote is dimmed, however it is marked
    // up, because the point of the dimming is to separate what this message
    // says from what it is replying to.
    let (quote, rest) = split_quote(raw);
    if !quote.is_empty() {
        let mut spans = vec![Span::styled(
            quote.to_string(),
            Style::default().fg(theme::DIM),
        )];
        spans.extend(inline(rest, Style::default().fg(theme::DIM), links));
        return Line::from(spans);
    }

    let trimmed = raw.trim_start();
    let indent = &raw[..raw.len() - trimmed.len()];

    // `---`, which our converter emits for `<hr>`. Drawn as a rule, since a
    // literal `---` in a mail body is nearly always one.
    if trimmed == "---" || trimmed == "***" || trimmed == "___" {
        return Line::from(Span::styled(
            "─".repeat(width.max(1)),
            Style::default().fg(theme::DIM),
        ));
    }

    if let Some(heading) = heading(trimmed) {
        return Line::from(inline(
            heading,
            Style::default()
                .fg(theme::HEADING)
                .add_modifier(Modifier::BOLD),
            links,
        ));
    }

    if let Some((bullet, text)) = list_item(trimmed) {
        let mut spans = vec![
            Span::raw(indent.to_string()),
            Span::styled(bullet, Style::default().fg(theme::ACCENT)),
        ];
        spans.extend(inline(text, Style::default(), links));
        return Line::from(spans);
    }

    let mut spans = vec![Span::raw(indent.to_string())];
    spans.extend(inline(trimmed, Style::default(), links));
    Line::from(spans)
}

/// Splits the leading `> ` markers from the rest of a line.
fn split_quote(line: &str) -> (&str, &str) {
    let mut end = 0;
    let bytes = line.as_bytes();
    loop {
        let rest = &line[end..];
        if let Some(after) = rest.strip_prefix("> ") {
            end = line.len() - after.len();
        } else if rest.starts_with('>') {
            end += 1;
        } else {
            break;
        }
        let _ = bytes;
    }
    line.split_at(end)
}

/// `## Heading` → `Heading`.
fn heading(line: &str) -> Option<&str> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    // A `#` must be followed by a space to be a heading, or `#4471` in a
    // subject line becomes one.
    line[hashes..].strip_prefix(' ').map(str::trim_start)
}

/// `- item` or `3. item` → the bullet to draw and the text after it.
fn list_item(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some(("• ".to_string(), rest));
    }
    let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits > 0 {
        if let Some(rest) = line[digits..].strip_prefix(". ") {
            return Some((format!("{}. ", &line[..digits]), rest));
        }
    }
    None
}

/// Reads the inline markers of one line into styled spans.
fn inline(text: &str, base: Style, links: &[String]) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut literal = String::new();
    let mut i = 0;

    // Closes off the plain run built so far.
    macro_rules! flush {
        () => {
            if !literal.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut literal), base));
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];

        // `![alt]`, which is what an image becomes: there is no URL, because
        // §9 never fetched one.
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some(close) = find(&chars, i + 2, "]") {
                flush!();
                let alt: String = chars[i + 2..close].iter().collect();
                spans.push(Span::styled(format!("[image: {alt}]"), base.fg(theme::DIM)));
                i = close + 1;
                continue;
            }
        }

        // `[text](url)`.
        if c == '[' {
            if let Some((label, href, end)) = link(&chars, i) {
                flush!();
                // The label is parsed too: a linked image is `[![alt]](url)`,
                // and the alt text is the only thing there is to show. It is
                // strictly shorter than what we are parsing, so this ends.
                spans.extend(inline(
                    &label,
                    base.fg(theme::LINK).add_modifier(Modifier::UNDERLINED),
                    links,
                ));
                // The number is the one `o` will offer, looked up rather than
                // counted, so the two cannot drift apart.
                if let Some(n) = links.iter().position(|u| *u == href) {
                    spans.push(Span::styled(format!(" [{}]", n + 1), base.fg(theme::DIM)));
                }
                i = end;
                continue;
            }
        }

        if let Some((marker, style)) = emphasis(&chars, i) {
            if let Some(close) = closing(&chars, i + marker.len(), marker) {
                flush!();
                let inner: String = chars[i + marker.len()..close].iter().collect();
                spans.push(Span::styled(inner, style(base)));
                i = close + marker.len();
                continue;
            }
        }

        literal.push(c);
        i += 1;
    }
    flush!();

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// An emphasis marker and what it does to a style.
type Emphasis = (&'static str, fn(Style) -> Style);

/// The emphasis marker starting at `i`, if one does.
///
/// Longest first: `**bold**` has to be recognised before the `*` inside it.
fn emphasis(chars: &[char], i: usize) -> Option<Emphasis> {
    let starts = |m: &str| chars[i..].starts_with(&m.chars().collect::<Vec<_>>()[..]);

    if starts("**") {
        return Some(("**", |s| s.add_modifier(Modifier::BOLD)));
    }
    if starts("`") {
        return Some(("`", |s| s.fg(theme::CODE)));
    }
    if starts("*") {
        return Some(("*", |s| s.add_modifier(Modifier::ITALIC)));
    }
    // `_` only at a word boundary, so `snake_case_names` survive intact.
    if starts("_") && !i.checked_sub(1).is_some_and(|p| is_word(chars[p])) {
        return Some(("_", |s| s.add_modifier(Modifier::ITALIC)));
    }
    None
}

/// Finds the closing half of an emphasis marker on this line.
///
/// Returns `None` when there is none, which is what keeps a lone `*` an
/// asterisk. The span may not begin or end against a space either: without
/// that, `2 * 3 * 4` renders `3` in italics.
fn closing(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    if chars.get(from).is_some_and(|c| c.is_whitespace()) {
        return None;
    }
    let m: Vec<char> = marker.chars().collect();
    let mut i = from;
    while i + m.len() <= chars.len() {
        if chars[i..i + m.len()] == m[..] {
            // Non-empty, and not closing against a space.
            if i > from && !chars[i - 1].is_whitespace() {
                // `_` closes only at a word boundary, as it opened at one.
                if marker == "_" && chars.get(i + 1).is_some_and(|c| is_word(*c)) {
                    i += 1;
                    continue;
                }
                return Some(i);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Reads `[label](href)` starting at `i`, returning the label, the href, and
/// the index just past the closing bracket.
fn link(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    let label_end = matching_bracket(chars, i)?;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let href_end = find(chars, label_end + 2, ")")?;
    let label: String = chars[i + 1..label_end].iter().collect();
    let href: String = chars[label_end + 2..href_end].iter().collect();
    if label.is_empty() || href.is_empty() {
        return None;
    }
    Some((label, href, href_end + 1))
}

/// The `]` that closes the `[` at `open`, counting nested pairs.
///
/// Nesting is not hypothetical here: a linked image is `[![alt]](url)`, which
/// is what a newsletter's logo becomes, and stopping at the first `]` reads
/// the label as the whole link and leaves the URL on screen as text.
fn matching_bracket(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in chars.iter().enumerate().skip(open) {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    (from..chars.len().saturating_sub(n.len() - 1)).find(|&i| chars[i..i + n.len()] == n[..])
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

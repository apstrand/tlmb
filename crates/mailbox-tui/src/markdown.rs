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
//! Wrapping happens here rather than in ratatui's `Paragraph`, for two reasons.
//! A wrapped quote has to keep its `> ` on every visual line — the same rule
//! the flattener in `mailbox-core` follows, and a quote whose second line lost
//! its marker is not a quote. And knowing where each link's label landed is
//! what makes an OSC 8 hyperlink possible at all: `Paragraph` decides that
//! internally, and ratatui strips control characters from anything handed to
//! it as text, so the escapes have to be written into the cells afterwards.
//!
//! Like [`crate::app`], the parsing is free of any terminal: it turns text into
//! [`BodyLine`]s that a test can read back.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// A run of text with one style, and the link it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub text: String,
    pub style: Style,
    /// Which of the body's links this run is the label of, if it is one.
    ///
    /// An index rather than the URL: it is the number shown beside the label
    /// and the number `o` offers, so carrying the index makes those the same
    /// fact rather than two facts that agree for now.
    pub link: Option<usize>,
}

impl Piece {
    pub fn new(text: impl Into<String>, style: Style) -> Piece {
        Piece {
            text: text.into(),
            style,
            link: None,
        }
    }

    fn width(&self) -> usize {
        self.text.width()
    }
}

/// One line of the body before wrapping.
#[derive(Debug, Clone, PartialEq)]
pub struct BodyLine {
    /// Drawn at the start of the line.
    pub lead: Piece,
    /// Drawn at the start of every visual line after the first: a quote's
    /// markers repeat, a bullet becomes the indentation that hangs under it.
    pub hang: Piece,
    pub pieces: Vec<Piece>,
}

impl BodyLine {
    pub fn plain(pieces: Vec<Piece>) -> BodyLine {
        BodyLine {
            lead: Piece::new("", Style::default()),
            hang: Piece::new("", Style::default()),
            pieces,
        }
    }

    /// The visible text, for tests and for anything that wants the line as a
    /// string.
    pub fn text(&self) -> String {
        let mut s = self.lead.text.clone();
        for p in &self.pieces {
            s.push_str(&p.text);
        }
        s
    }
}

/// Renders a Markdown body into styled lines.
///
/// `links` is the body's own link list, and a link's position in it is the
/// number shown beside it — so what the reader sees next to a link is exactly
/// what `o` will offer them.
pub fn body_lines(md: &str, width: usize, links: &[String]) -> Vec<BodyLine> {
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
            out.push(BodyLine::plain(vec![Piece::new(
                raw.to_string(),
                Style::default().fg(theme::CODE),
            )]));
            continue;
        }
        out.push(block(raw, width, links));
    }
    out
}

/// One line, after its block-level markers have been read.
fn block(raw: &str, width: usize, links: &[String]) -> BodyLine {
    // Quoting first: everything inside a quote is dimmed, however it is marked
    // up, because the point of the dimming is to separate what this message
    // says from what it is replying to.
    let (quote, rest) = split_quote(raw);
    if !quote.is_empty() {
        let marker = Piece::new(quote.to_string(), Style::default().fg(theme::DIM));
        return BodyLine {
            lead: marker.clone(),
            // Repeated, not indented away: a wrapped quote whose second line
            // has no `>` reads as the reply rather than the quotation.
            hang: marker,
            pieces: inline(rest, Style::default().fg(theme::DIM), links),
        };
    }

    let trimmed = raw.trim_start();
    let indent = &raw[..raw.len() - trimmed.len()];

    // `---`, which our converter emits for `<hr>`. Drawn as a rule, since a
    // literal `---` in a mail body is nearly always one.
    if trimmed == "---" || trimmed == "***" || trimmed == "___" {
        return BodyLine::plain(vec![Piece::new(
            "─".repeat(width.max(1)),
            Style::default().fg(theme::DIM),
        )]);
    }

    if let Some(heading) = heading(trimmed) {
        return BodyLine::plain(inline(
            heading,
            Style::default()
                .fg(theme::HEADING)
                .add_modifier(Modifier::BOLD),
            links,
        ));
    }

    if let Some((bullet, text)) = list_item(trimmed) {
        // The continuation hangs under the text, not under the bullet, so a
        // wrapped item still looks like one item.
        let hang = " ".repeat(indent.width() + bullet.width());
        return BodyLine {
            lead: Piece::new(
                format!("{indent}{bullet}"),
                Style::default().fg(theme::ACCENT),
            ),
            hang: Piece::new(hang, Style::default()),
            pieces: inline(text, Style::default(), links),
        };
    }

    BodyLine {
        lead: Piece::new(indent.to_string(), Style::default()),
        // Indented text is usually preformatted — our converter indents nested
        // lists, and mail indents code — so a wrapped line keeps the indent.
        hang: Piece::new(indent.to_string(), Style::default()),
        pieces: inline(trimmed, Style::default(), links),
    }
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
fn inline(text: &str, base: Style, links: &[String]) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Piece> = Vec::new();
    let mut literal = String::new();
    let mut i = 0;

    // Closes off the plain run built so far.
    macro_rules! flush {
        () => {
            if !literal.is_empty() {
                spans.push(Piece::new(std::mem::take(&mut literal), base));
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
                spans.push(Piece::new(format!("[image: {alt}]"), base.fg(theme::DIM)));
                i = close + 1;
                continue;
            }
        }

        // `[text](url)`.
        if c == '[' {
            if let Some((label, href, end)) = link(&chars, i) {
                flush!();
                // Looked up rather than counted, so what is shown beside a
                // link and what `o` offers cannot drift apart.
                let index = links.iter().position(|u| *u == href);
                // The label is parsed too: a linked image is `[![alt]](url)`,
                // and the alt text is the only thing there is to show. It is
                // strictly shorter than what we are parsing, so this ends.
                for mut piece in inline(
                    &label,
                    base.fg(theme::LINK).add_modifier(Modifier::UNDERLINED),
                    links,
                ) {
                    piece.link = index;
                    spans.push(piece);
                }
                if let Some(n) = index {
                    spans.push(Piece::new(format!(" [{}]", n + 1), base.fg(theme::DIM)));
                }
                i = end;
                continue;
            }
        }

        if let Some((marker, style)) = emphasis(&chars, i) {
            if let Some(close) = closing(&chars, i + marker.len(), marker) {
                flush!();
                let inner: String = chars[i + marker.len()..close].iter().collect();
                spans.push(Piece::new(inner, style(base)));
                i = close + marker.len();
                continue;
            }
        }

        literal.push(c);
        i += 1;
    }
    flush!();

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

/// Breaks lines to `width`, repeating each line's hanging prefix.
///
/// Greedy and word-based, which is what every mail client does and what a
/// reader expects. A word too long for a line — a URL written out in prose — is
/// broken rather than allowed to run off the edge.
pub fn wrap(lines: &[BodyLine], width: usize) -> Vec<Vec<Piece>> {
    let mut out: Vec<Vec<Piece>> = Vec::new();

    for line in lines {
        let mut prefix = line.lead.clone();
        let mut current: Vec<Piece> = Vec::new();
        let mut used = prefix.width();
        let avail = width.max(prefix.width() + 1);
        // Whitespace at a line break disappears with the break; whitespace at
        // the *start* of the line is indentation and stays. Without the
        // distinction, an indented code line and a header's aligned label both
        // slide left by however many spaces they began with.
        let mut at_start = true;

        // Nothing on the line: an empty line is still a line.
        if line.pieces.iter().all(|p| p.text.is_empty()) {
            out.push(vec![prefix]);
            continue;
        }

        for piece in &line.pieces {
            for token in tokens(&piece.text) {
                let w = token.width();
                let space = token.chars().all(char::is_whitespace);

                if space && current.is_empty() && !at_start {
                    continue;
                }
                if used + w > avail && !current.is_empty() {
                    out.push(finish(&mut current, &mut prefix, &line.hang));
                    used = prefix.width();
                    at_start = false;
                    if space {
                        continue;
                    }
                }
                // Still too long with a line to itself: hard-break it.
                if used + w > avail {
                    let rest = avail.saturating_sub(prefix.width()).max(1);
                    for chunk in break_word(&token, avail.saturating_sub(used).max(1), rest) {
                        let chunk_w = chunk.width();
                        if used + chunk_w > avail && !current.is_empty() {
                            out.push(finish(&mut current, &mut prefix, &line.hang));
                            used = prefix.width();
                            at_start = false;
                        }
                        used += chunk_w;
                        push(&mut current, &chunk, piece);
                    }
                    continue;
                }
                used += w;
                push(&mut current, &token, piece);
            }
        }
        out.push(finish(&mut current, &mut prefix, &line.hang));
    }
    out
}

/// Completes a visual line and prepares the prefix for the next one.
fn finish(current: &mut Vec<Piece>, prefix: &mut Piece, hang: &Piece) -> Vec<Piece> {
    let mut line = vec![std::mem::replace(prefix, hang.clone())];
    line.append(current);
    // A trailing space at a break is invisible, and would push a link's
    // underline one cell past its text.
    if let Some(last) = line.last_mut() {
        while last.text.ends_with(' ') {
            last.text.pop();
        }
    }
    line
}

/// Appends text to the run being built, merging with the previous run when it
/// carries the same style — so a link's label is one run and gets one
/// hyperlink rather than one per word.
fn push(current: &mut Vec<Piece>, text: &str, from: &Piece) {
    match current.last_mut() {
        Some(last) if last.style == from.style && last.link == from.link => {
            last.text.push_str(text);
        }
        _ => current.push(Piece {
            text: text.to_string(),
            style: from.style,
            link: from.link,
        }),
    }
}

/// Splits into words and the runs of spaces between them, keeping both.
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_space = false;

    for c in text.chars() {
        let space = c.is_whitespace();
        if !current.is_empty() && space != in_space {
            out.push(std::mem::take(&mut current));
        }
        in_space = space;
        current.push(c);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Cuts an over-long word into pieces that fit, on character boundaries.
///
/// `first` is what is left of the line it starts on; `rest` is a whole line,
/// since every chunk after the first begins one.
fn break_word(word: &str, first: usize, rest: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut budget = first.max(1);

    for c in word.chars() {
        let w = c.to_string().width();
        if current.width() + w > budget && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            budget = rest.max(1);
        }
        current.push(c);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Where a link's label ended up on screen, so the shell can mark it up as a
/// terminal hyperlink afterwards.
///
/// Afterwards, and not as part of the drawing, because ratatui gives no way to
/// do it during: text handed to a buffer has its control characters filtered
/// out (`Buffer::set_stringn`), and writing the escapes into a cell's symbol
/// instead makes `Buffer::diff` measure the URL as though it were visible and
/// skip the cells after it. So the escapes are written straight at the terminal
/// once the frame is on screen, at coordinates this layout recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSpan {
    pub x: u16,
    pub y: u16,
    /// The label as drawn — reprinted under the hyperlink, so it has to match
    /// what is already there.
    pub text: String,
    pub style: Style,
    pub url: String,
}

/// Draws a wrapped body.
pub struct Body<'a> {
    pub lines: &'a [BodyLine],
    pub links: &'a [String],
    /// How many visual lines are scrolled off the top.
    pub scroll: u16,
    /// Filled with the position of every link label drawn, for the shell to
    /// turn into hyperlinks.
    pub found: Option<&'a mut Vec<LinkSpan>>,
}

impl Widget for Body<'_> {
    fn render(mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let wrapped = wrap(self.lines, area.width as usize);

        for (row, line) in wrapped
            .iter()
            .skip(self.scroll as usize)
            .take(area.height as usize)
            .enumerate()
        {
            let y = area.y + row as u16;
            let mut x = area.x;
            for piece in line {
                if x >= area.right() {
                    break;
                }
                let start = x;
                let remaining = (area.right() - x) as usize;
                let (end, _) = buf.set_stringn(x, y, &piece.text, remaining, piece.style);
                x = end;

                let Some(found) = self.found.as_mut() else {
                    continue;
                };
                let Some(url) = piece.link.and_then(|i| self.links.get(i)) else {
                    continue;
                };
                if end > start {
                    found.push(LinkSpan {
                        x: start,
                        y,
                        // What was actually drawn, which is what has to be
                        // reprinted: a label clipped by the pane's edge is
                        // shorter than the piece.
                        text: clip(&piece.text, (end - start) as usize),
                        style: piece.style,
                        url: url.clone(),
                    });
                }
            }
        }
    }
}

/// The prefix of `text` that occupies `width` columns.
fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let w = c.to_string().width();
        if out.width() + w > width {
            break;
        }
        out.push(c);
    }
    out
}

/// Reprints each label as an OSC 8 hyperlink, in place.
///
/// The cells already hold the right characters; what they lack is the escape
/// that tells the terminal a URL is behind them, which is what makes
/// right-click offer "copy link" and ctrl-click follow it. So each label is
/// written again at the same position, wrapped in the sequence and carrying
/// the style it was drawn with.
///
/// Reprinting rather than annotating is forced: OSC 8 applies to text as it is
/// written, and there is no way to attach a URL to characters already on
/// screen. It costs a few short writes per frame, and frames only happen when
/// something changed.
pub fn write_hyperlinks(out: &mut impl std::io::Write, links: &[LinkSpan]) -> std::io::Result<()> {
    for (i, link) in links.iter().enumerate() {
        // A URL carrying the sequence terminator would end the hyperlink early
        // and put the rest of itself on screen as text.
        if link.url.contains(['\x1b', '\x07']) || link.url.is_empty() {
            continue;
        }
        // The id lets a terminal treat a label split across two lines as one
        // link, so hovering either half highlights both.
        write!(
            out,
            "\x1b[{};{}H\x1b]8;id=mailbox{i};{}\x1b\\{}{}\x1b]8;;\x1b\\\x1b[0m",
            link.y + 1,
            link.x + 1,
            link.url,
            sgr(link.style),
            link.text,
        )?;
    }
    out.flush()
}

/// A style as the escape sequence that sets it.
///
/// Only the attributes this interface actually uses. Written by hand rather
/// than through crossterm because it has to sit inside a hand-written OSC 8
/// sequence anyway, and one place that knows the mapping beats two.
pub fn sgr(style: Style) -> String {
    let mut codes: Vec<String> = vec!["0".to_string()];

    if let Some(fg) = style.fg.and_then(colour_code) {
        codes.push(fg.to_string());
    }
    if let Some(bg) = style.bg.and_then(colour_code) {
        codes.push((bg + 10).to_string());
    }
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
    ] {
        if style.add_modifier.contains(modifier) {
            codes.push(code.to_string());
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

/// The SGR foreground number for one of the terminal's own sixteen colours.
fn colour_code(colour: ratatui::style::Color) -> Option<u16> {
    use ratatui::style::Color::*;
    Some(match colour {
        Black => 30,
        Red => 31,
        Green => 32,
        Yellow => 33,
        Blue => 34,
        Magenta => 35,
        Cyan => 36,
        Gray => 37,
        DarkGray => 90,
        LightRed => 91,
        LightGreen => 92,
        LightYellow => 93,
        LightBlue => 94,
        LightMagenta => 95,
        LightCyan => 96,
        White => 97,
        // Indexed and Rgb are not used by this interface's palette, which is
        // deliberately the terminal's own sixteen (see `theme`).
        _ => return None,
    })
}

/// Whether this terminal should be sent OSC 8 hyperlinks.
///
/// On by default: terminals that do not understand the sequence discard it, and
/// it has been widely supported since 2017. `MAILBOX_HYPERLINKS=0` turns it off
/// for the ones that do not, and a `dumb` terminal never gets it.
pub fn hyperlinks_wanted() -> bool {
    match std::env::var("MAILBOX_HYPERLINKS").as_deref() {
        Ok("0") | Ok("no") | Ok("false") => return false,
        Ok(_) => return true,
        Err(_) => {}
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb") | Err(_))
}

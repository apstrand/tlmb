//! Turning a message into the renderings §6.4 promises: text, Markdown for LLM
//! consumers, sanitized HTML, and the quote-stripped forms.
//!
//! The HTML here is treated as hostile throughout (§9): no remote content, no
//! scripts, no automatic loads. The Markdown converter is deliberately small —
//! mail HTML is table soup, and a converter that tries to be faithful to it
//! produces worse Markdown than one that throws the layout away.

use mailbox_api::{Body, BodyFormat};

use crate::mime::Parsed;

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub format: BodyFormat,
    /// `--text-only`: never fall back to HTML, even when there is no text part.
    pub text_only: bool,
    /// `--strip-quotes`: drop quoted replies and signatures (§6.4).
    pub strip_quotes: bool,
    /// `--max-body-bytes N`; `None` for unlimited.
    pub max_body_bytes: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            format: BodyFormat::Text,
            text_only: false,
            strip_quotes: false,
            max_body_bytes: None,
        }
    }
}

/// Renders a parsed message's body under `opts`.
///
/// `raw` is needed only for [`BodyFormat::Raw`], which emits the exact RFC5322
/// bytes (§13) — lossily decoded, since JSON cannot carry arbitrary bytes.
pub fn body(parsed: &Parsed, raw: Option<&[u8]>, opts: &RenderOptions) -> Option<Body> {
    let content = match opts.format {
        BodyFormat::Raw => String::from_utf8_lossy(raw?).into_owned(),
        BodyFormat::Html => parsed.html.clone().or_else(|| {
            // A text-only message still has to answer `--format=html`
            // truthfully; escaping it is the honest conversion.
            parsed
                .text
                .as_ref()
                .map(|t| format!("<pre>{}</pre>", escape_html(t)))
        })?,
        // `--text-only` refuses a body that only exists because HTML was
        // flattened; without it, Markdown of the HTML is the better rendering.
        BodyFormat::Md => match (&parsed.text, &parsed.html) {
            (Some(t), _) if opts.text_only && !parsed.text_is_derived => t.clone(),
            _ if opts.text_only => return None,
            (_, Some(h)) => html_to_markdown(h),
            (Some(t), None) => t.clone(),
            (None, None) => return None,
        },
        BodyFormat::Text => match (&parsed.text, &parsed.html) {
            (Some(t), _) if !(opts.text_only && parsed.text_is_derived) => t.clone(),
            (None, Some(h)) if !opts.text_only => mail_parser::decoders::html::html_to_text(h),
            _ => return None,
        },
    };

    let content = if opts.strip_quotes && opts.format != BodyFormat::Raw {
        strip_quotes(&content)
    } else {
        content
    };

    let (content, truncated) = match opts.max_body_bytes {
        Some(max) if content.len() > max => (truncate_utf8(&content, max).to_string(), true),
        _ => (content, false),
    };

    Some(Body {
        format: opts.format,
        content,
        truncated,
    })
}

/// Cuts at a character boundary at or below `max`, so truncation never produces
/// invalid UTF-8 for a caller that counts bytes.
pub fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Drops quoted replies and signatures (§6.4's `--strip-quotes`).
///
/// Three signals, in the order they are trustworthy: the RFC 3676 signature
/// delimiter `-- `, a run of `>`-prefixed lines, and an attribution line
/// immediately above one. Anything more aggressive starts eating real prose in
/// bottom-posted mail, which is the failure the flag exists to avoid.
pub fn strip_quotes(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut end = lines.len();

    // A signature delimiter truncates everything after it.
    if let Some(pos) = lines
        .iter()
        .position(|l| l.trim_end() == "--" || *l == "-- ")
    {
        end = pos;
    }

    let mut out: Vec<&str> = Vec::with_capacity(end);
    let mut i = 0;
    while i < end {
        let line = lines[i];
        if is_quote(line) {
            // Skip the whole quoted run, and the attribution line above it.
            while i < end && (is_quote(lines[i]) || lines[i].trim().is_empty()) {
                i += 1;
            }
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            if out.last().is_some_and(|l| is_attribution(l)) {
                out.pop();
            }
            continue;
        }
        out.push(line);
        i += 1;
    }

    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn is_quote(line: &str) -> bool {
    line.trim_start().starts_with('>')
}

/// "On Tue, 1 Aug 2026, Alice wrote:" and its many dialects. Matching the
/// shape — starts with a temporal or address word, ends in a colon — rather
/// than the wording, since the wording is localized.
fn is_attribution(line: &str) -> bool {
    let t = line.trim();
    if !t.ends_with(':') || t.len() > 200 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    lower.starts_with("on ")
        || lower.starts_with("am ")
        || lower.starts_with("le ")
        || lower.starts_with("el ")
        || lower.contains(" wrote:")
        || lower.contains(" schrieb:")
        || lower.contains(" a écrit:")
        || lower.starts_with("from:")
        || lower.starts_with("-----original message-----")
}

/// A small, deliberately lossy HTML-to-Markdown converter for `show
/// --format=md` (§6.4).
///
/// It keeps the things that carry meaning in mail — headings, emphasis, links,
/// lists, quotes, code — and discards layout entirely. Tables become their cell
/// text, one row per line: mail tables are almost always layout scaffolding,
/// and a faithful Markdown table of a layout table is noise that costs an LLM
/// consumer tokens for nothing.
pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut quote_depth = 0usize;
    let mut pending_link: Option<String> = None;
    let mut link_text_start = 0usize;
    let mut in_pre = false;

    for token in tokenize(html) {
        match token {
            Token::Text(t) => {
                let text = decode_entities(&t);
                if in_pre {
                    out.push_str(&text);
                } else {
                    push_inline(&mut out, &text);
                }
            }
            Token::Open(tag, attrs) => match tag.as_str() {
                // Dropped whole, contents and all — see the skip in tokenize().
                "script" | "style" | "head" => {}
                "br" => out.push('\n'),
                "hr" => {
                    ensure_block(&mut out);
                    out.push_str("---\n\n");
                }
                "p" | "div" | "section" | "article" | "tr" => ensure_block(&mut out),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    ensure_block(&mut out);
                    let level = tag[1..].parse::<usize>().unwrap_or(1);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                }
                "strong" | "b" => out.push_str("**"),
                "em" | "i" => out.push('*'),
                "code" if !in_pre => out.push('`'),
                "pre" => {
                    ensure_block(&mut out);
                    out.push_str("```\n");
                    in_pre = true;
                }
                "blockquote" => {
                    quote_depth += 1;
                    ensure_block(&mut out);
                    out.push_str(&"> ".repeat(quote_depth));
                }
                "ul" => {
                    ensure_block(&mut out);
                    list_stack.push(ListKind::Unordered);
                }
                "ol" => {
                    ensure_block(&mut out);
                    list_stack.push(ListKind::Ordered(0));
                }
                "li" => {
                    trim_trailing_spaces(&mut out);
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                    out.push_str(&indent);
                    match list_stack.last_mut() {
                        Some(ListKind::Ordered(n)) => {
                            *n += 1;
                            out.push_str(&format!("{n}. "));
                        }
                        _ => out.push_str("- "),
                    }
                }
                "a" => {
                    // `mailto:` and `#fragment` links carry no information a
                    // reader needs repeated; plain text is kinder.
                    let href = attrs.get("href").cloned().unwrap_or_default();
                    if !href.is_empty() && !href.starts_with('#') {
                        pending_link = Some(href);
                        link_text_start = out.len();
                        out.push('[');
                    }
                }
                "img" => {
                    // §9: images are never loaded. The alt text is the whole of
                    // what we can honestly show.
                    let alt = attrs.get("alt").cloned().unwrap_or_default();
                    if !alt.is_empty() {
                        out.push_str(&format!("![{}]", decode_entities(&alt)));
                    }
                }
                // Cells are separated by a space, not by table syntax: mail
                // tables are layout, and a Markdown table of a layout table
                // costs an LLM consumer tokens for nothing.
                "td" | "th" if !out.ends_with(' ') && !out.ends_with('\n') => out.push(' '),
                _ => {}
            },
            Token::Close(tag) => match tag.as_str() {
                "strong" | "b" => out.push_str("**"),
                "em" | "i" => out.push('*'),
                "code" if !in_pre => out.push('`'),
                "pre" => {
                    in_pre = false;
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```\n\n");
                }
                "blockquote" => {
                    quote_depth = quote_depth.saturating_sub(1);
                    ensure_block(&mut out);
                }
                "ul" | "ol" => {
                    list_stack.pop();
                    ensure_block(&mut out);
                }
                "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article"
                | "tr" => ensure_block(&mut out),
                // A list item ends the line but not the block: a blank line
                // between every bullet turns a five-item list into a page.
                "li" => {
                    trim_trailing_spaces(&mut out);
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                "a" => {
                    if let Some(href) = pending_link.take() {
                        // A link whose text was empty would render as `[](url)`,
                        // which reads worse than the bare URL.
                        if out.len() == link_text_start + 1 {
                            out.truncate(link_text_start);
                            out.push_str(&href);
                        } else {
                            out.push_str(&format!("]({href})"));
                        }
                    }
                }
                _ => {}
            },
        }
    }

    // Collapse the runs of blank lines the block handling leaves behind.
    let mut result = String::with_capacity(out.len());
    let mut blanks = 0;
    for line in out.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        result.push_str(trimmed);
        result.push('\n');
    }
    result.trim().to_string()
}

enum ListKind {
    Unordered,
    Ordered(usize),
}

fn ensure_block(out: &mut String) {
    trim_trailing_spaces(out);
    if out.is_empty() {
        return;
    }
    if !out.ends_with("\n\n") {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
}

fn trim_trailing_spaces(out: &mut String) {
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
}

/// Appends text, collapsing the whitespace HTML would have collapsed.
fn push_inline(out: &mut String, text: &str) {
    let mut last_space = out.ends_with(char::is_whitespace) || out.is_empty();
    for c in text.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
}

enum Token {
    Text(String),
    Open(String, std::collections::HashMap<String, String>),
    Close(String),
}

/// A forgiving HTML tokenizer.
///
/// Not a parser: mail HTML is rarely well-formed, and a strict parser's failure
/// mode is an empty rendering. Unbalanced tags here just produce slightly wrong
/// Markdown, which is the right way to fail (§14.4).
fn tokenize(html: &str) -> Vec<Token> {
    let b: Vec<char> = html.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut text = String::new();

    while i < b.len() {
        if b[i] != '<' {
            text.push(b[i]);
            i += 1;
            continue;
        }
        if !text.is_empty() {
            out.push(Token::Text(std::mem::take(&mut text)));
        }
        // Comments and doctypes.
        if b[i..].starts_with(&['<', '!']) {
            match find(&b, i, "-->").or_else(|| find(&b, i, ">")) {
                Some(end) => {
                    i = end;
                    continue;
                }
                None => break,
            }
        }
        let Some(end) = find(&b, i, ">") else { break };
        let raw: String = b[i + 1..end - 1].iter().collect();
        i = end;

        let closing = raw.starts_with('/');
        let raw = raw.trim_start_matches('/').trim_end_matches('/');
        let mut parts = raw.splitn(2, |c: char| c.is_whitespace());
        let name = parts.next().unwrap_or("").to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }

        if closing {
            out.push(Token::Close(name));
            continue;
        }

        // §9 again: script and style content is never rendered, so skip past
        // the closing tag entirely rather than emitting its text.
        if name == "script" || name == "style" {
            let close = format!("</{name}>");
            if let Some(end) = find_ci(&b, i, &close) {
                i = end;
            }
            continue;
        }

        out.push(Token::Open(name, parse_attrs(parts.next().unwrap_or(""))));
    }
    if !text.is_empty() {
        out.push(Token::Text(text));
    }
    out
}

/// Returns the index just past `needle`.
fn find(hay: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    (from..hay.len().saturating_sub(n.len() - 1))
        .find(|&i| hay[i..i + n.len()] == n[..])
        .map(|i| i + n.len())
}

fn find_ci(hay: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.to_ascii_lowercase().chars().collect();
    (from..hay.len().saturating_sub(n.len() - 1))
        .find(|&i| {
            hay[i..i + n.len()]
                .iter()
                .zip(&n)
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
        })
        .map(|i| i + n.len())
}

fn parse_attrs(s: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if start == i {
            break;
        }
        let name: String = chars[start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            let quote = if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let q = chars[i];
                i += 1;
                Some(q)
            } else {
                None
            };
            let vstart = i;
            while i < chars.len() {
                match quote {
                    Some(q) if chars[i] == q => break,
                    None if chars[i].is_whitespace() => break,
                    _ => i += 1,
                }
            }
            let value: String = chars[vstart..i].iter().collect();
            if quote.is_some() {
                i += 1;
            }
            out.insert(name, decode_entities(&value));
        } else {
            out.insert(name, String::new());
        }
    }
    out
}

/// The named entities that actually appear in mail, plus numeric ones.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let Some(semi) = chars[i..]
            .iter()
            .position(|c| *c == ';')
            .filter(|p| *p <= 10)
        else {
            out.push('&');
            i += 1;
            continue;
        };
        let entity: String = chars[i + 1..i + semi].iter().collect();
        let replacement = match entity.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "rsquo" | "#8217" => Some('\u{2019}'),
            "lsquo" => Some('\u{2018}'),
            "ldquo" => Some('\u{201c}'),
            "rdquo" => Some('\u{201d}'),
            other => other
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hexdigits) => u32::from_str_radix(hexdigits, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(c) => {
                out.push(c);
                i += semi + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

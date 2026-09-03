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
    let mut links: Vec<String> = Vec::new();
    let content = match opts.format {
        BodyFormat::Raw => String::from_utf8_lossy(raw?).into_owned(),
        // §9: what comes back is the safe subset, never the sender's markup.
        // Sanitizing here rather than at each renderer means a frontend cannot
        // forget to do it — and every frontend that displays HTML is one that
        // could.
        BodyFormat::Html => parsed.html.as_deref().map(sanitize_html).or_else(|| {
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
            (_, Some(h)) => {
                let flat = render_html(h, Flavour::Markdown, LinkStyle::Inline);
                links = flat.links;
                flat.text
            }
            (Some(t), None) => t.clone(),
            (None, None) => return None,
        },
        BodyFormat::Text => match (&parsed.text, &parsed.html) {
            (Some(t), _) if !(opts.text_only && parsed.text_is_derived) => t.clone(),
            (None, Some(h)) if !opts.text_only => {
                let flat = flatten_html(h);
                links = flat.links;
                flat.text
            }
            _ => return None,
        },
    };

    // Anything the reader can see but the footnotes did not number: a URL
    // written out as itself, in either a plain-text mail or an HTML one. Listed
    // after the footnotes so `[2]` in the text stays `links[1]`.
    for url in bare_urls(&content) {
        if !links.contains(&url) {
            links.push(url);
        }
    }

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
        links,
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

/// What becomes of a hyperlink when HTML is flattened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStyle {
    /// `[text](url)` — Markdown.
    Inline,
    /// `text [1]`, with the URLs collected under the body.
    ///
    /// The default for reading mail. A newsletter's links are often the entire
    /// content of the message, and inlining a 200-character tracking URL in the
    /// middle of a sentence destroys the sentence — so the text stays readable
    /// and the URLs go where they can be read, or copied, deliberately.
    Footnotes,
    /// The link text alone. For previews and the search index, where a URL is
    /// noise in a 120-character snippet.
    Drop,
}

/// Which output the walker is producing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavour {
    /// Markdown, for `--format=md` and its LLM consumers (§6.4).
    Markdown,
    /// Flat text, for `--format=text` and everything that shows a body to a
    /// human.
    Plain,
}

/// A small, deliberately lossy HTML converter, used for both `show
/// --format=md` and the flattening of an HTML-only message for `--format=text`
/// (§6.4).
///
/// It keeps the things that carry meaning in mail — headings, emphasis, links,
/// lists, quotes, code — and discards layout entirely. Tables become their cell
/// text, one row per line: mail tables are almost always layout scaffolding,
/// and a faithful table of a layout table is noise that costs a reader
/// attention and an LLM consumer tokens for nothing.
///
/// One walker serves both because the hard parts — collapsing whitespace the
/// way HTML would, keeping a quote marker on every line of a quoted block,
/// nesting lists — are the same problem in either output, and two copies of
/// them would drift.
pub fn html_to_markdown(html: &str) -> String {
    render_html(html, Flavour::Markdown, LinkStyle::Inline).text
}

/// Flattens HTML to text, keeping the links.
///
/// This exists because the obvious alternative — `mail_parser`'s
/// `html_to_text` — drops every `href`, and in a great deal of real mail the
/// links *are* the message: an order confirmation whose "track your parcel"
/// leads nowhere is not a rendering of that mail, it is a summary of it.
pub fn html_to_text(html: &str) -> String {
    render_html(html, Flavour::Plain, LinkStyle::Footnotes).text
}

/// The same flattening, keeping the numbered destinations as data.
///
/// The numbers in [`Flattened::links`] are the numbers in the text: `[2]` in
/// the body is `links[1]`. That correspondence is what lets an interface offer
/// "open link 2" without parsing its own output back.
pub fn flatten_html(html: &str) -> Flattened {
    render_html(html, Flavour::Plain, LinkStyle::Footnotes)
}

/// A flattened HTML body and the destinations it referred to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flattened {
    pub text: String,
    /// Deduplicated, in the order the reader meets them.
    pub links: Vec<String>,
}

/// Flattens HTML to text with no link URLs at all.
///
/// For the preview line and the FTS text, where the URLs would crowd out the
/// words someone is actually going to read or search for.
pub fn html_to_plain(html: &str) -> String {
    render_html(html, Flavour::Plain, LinkStyle::Drop).text
}

fn render_html(html: &str, flavour: Flavour, links: LinkStyle) -> Flattened {
    let mut out = Out::new();
    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut quote_depth = 0usize;
    let mut pending_link: Option<String> = None;
    let mut link_text_start = 0usize;
    let mut in_pre = false;
    let mut refs = Footnotes::default();

    for token in tokenize(html) {
        match token {
            Token::Text(t) => {
                let text = decode_entities(&t);
                if in_pre {
                    out.push_verbatim(&text);
                } else {
                    out.push_inline(&text);
                }
            }
            Token::Open(tag, attrs) => match tag.as_str() {
                // Dropped whole, contents and all — see the skip in tokenize().
                "script" | "style" | "head" => {}
                "br" => out.newline(),
                "hr" => {
                    out.ensure_block();
                    out.push_raw("---");
                    out.ensure_block();
                }
                "p" | "div" | "section" | "article" | "tr" => out.ensure_block(),
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    out.ensure_block();
                    if flavour == Flavour::Markdown {
                        let level = tag[1..].parse::<usize>().unwrap_or(1);
                        out.push_raw(&"#".repeat(level));
                        out.push_raw(" ");
                    }
                }
                "strong" | "b" => out.push_raw(flavour.strong()),
                "em" | "i" => out.push_raw(flavour.emphasis()),
                "code" if !in_pre => out.push_raw(flavour.code()),
                "pre" => {
                    out.ensure_block();
                    if flavour == Flavour::Markdown {
                        out.push_raw("```");
                        out.newline();
                    }
                    in_pre = true;
                }
                "blockquote" => {
                    quote_depth += 1;
                    out.set_quote(quote_depth);
                    out.ensure_block();
                }
                "ul" => {
                    out.ensure_block();
                    list_stack.push(ListKind::Unordered);
                }
                "ol" => {
                    out.ensure_block();
                    list_stack.push(ListKind::Ordered(0));
                }
                "li" => {
                    out.end_line();
                    let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                    out.push_raw(&indent);
                    match list_stack.last_mut() {
                        Some(ListKind::Ordered(n)) => {
                            *n += 1;
                            out.push_raw(&format!("{n}. "));
                        }
                        _ => out.push_raw("- "),
                    }
                }
                "a" => {
                    // A `#fragment` points inside a document we are not
                    // showing, so it can only mislead.
                    let href = attrs.get("href").cloned().unwrap_or_default();
                    let href = decode_entities(href.trim());
                    if !href.is_empty() && !href.starts_with('#') && !is_dangerous_url(&href) {
                        pending_link = Some(href);
                        link_text_start = out.len();
                        if links == LinkStyle::Inline {
                            out.push_raw("[");
                        }
                    }
                }
                "img" => {
                    // §9: images are never loaded. The alt text is the whole of
                    // what we can honestly show, and saying an image was here
                    // beats a silent gap — a mail whose only content is one
                    // image should not render as nothing at all.
                    let alt = decode_entities(attrs.get("alt").map(String::as_str).unwrap_or(""));
                    let alt = alt.trim();
                    match flavour {
                        Flavour::Markdown if !alt.is_empty() => {
                            out.push_raw(&format!("![{alt}]"));
                        }
                        Flavour::Markdown => {}
                        Flavour::Plain if !alt.is_empty() => {
                            out.push_inline(&format!("[image: {alt}]"));
                        }
                        Flavour::Plain => out.push_inline("[image]"),
                    }
                }
                // Cells are separated by a space, not by table syntax.
                "td" | "th" => out.separate(),
                _ => {}
            },
            Token::Close(tag) => match tag.as_str() {
                "strong" | "b" => out.push_raw(flavour.strong()),
                "em" | "i" => out.push_raw(flavour.emphasis()),
                "code" if !in_pre => out.push_raw(flavour.code()),
                "pre" => {
                    in_pre = false;
                    out.end_line();
                    if flavour == Flavour::Markdown {
                        out.push_raw("```");
                    }
                    out.ensure_block();
                }
                "blockquote" => {
                    quote_depth = quote_depth.saturating_sub(1);
                    out.end_line();
                    out.set_quote(quote_depth);
                    out.ensure_block();
                }
                "ul" | "ol" => {
                    list_stack.pop();
                    out.ensure_block();
                }
                "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article"
                | "tr" => out.ensure_block(),
                // A list item ends the line but not the block: a blank line
                // between every bullet turns a five-item list into a page.
                "li" => out.end_line(),
                "a" => {
                    if let Some(href) = pending_link.take() {
                        close_link(&mut out, &mut refs, links, link_text_start, &href);
                    }
                }
                _ => {}
            },
        }
    }

    let mut text = out.finish();
    // Only the footnote style prints the list; Markdown already carries each
    // destination beside its label, and printing both would say everything
    // twice.
    if links == LinkStyle::Footnotes {
        if let Some(list) = refs.render() {
            text.push_str(&list);
        }
    }
    Flattened {
        text,
        links: refs.urls,
    }
}

impl Flavour {
    fn strong(self) -> &'static str {
        match self {
            Flavour::Markdown => "**",
            Flavour::Plain => "",
        }
    }
    fn emphasis(self) -> &'static str {
        match self {
            Flavour::Markdown => "*",
            Flavour::Plain => "",
        }
    }
    fn code(self) -> &'static str {
        match self {
            Flavour::Markdown => "`",
            Flavour::Plain => "",
        }
    }
}

/// Emits the closing half of an `<a>`, in whichever style was asked for.
fn close_link(
    out: &mut Out,
    refs: &mut Footnotes,
    style: LinkStyle,
    text_start: usize,
    href: &str,
) {
    let opened = usize::from(style == LinkStyle::Inline);
    let text = out.since(text_start + opened).trim().to_string();

    // An empty label — an image link, a tracking pixel wrapped in an anchor —
    // would render as `[](url)` or as a bare footnote marker attached to
    // nothing. The URL itself is the only honest label left.
    if text.is_empty() {
        out.truncate(text_start);
        out.push_inline(href);
        return;
    }

    // The label already *is* the destination, which is how most plain links
    // and every `mailto:` written out in full arrive. Repeating it helps
    // nobody.
    if text.eq_ignore_ascii_case(href) || href.eq_ignore_ascii_case(&format!("mailto:{text}")) {
        if style == LinkStyle::Inline {
            out.truncate(text_start);
            out.push_inline(&text);
        }
        return;
    }

    match style {
        // Numbered even though the number is not printed here: `Body.links` is
        // the same list in the same order whichever rendering asked for it, so
        // an interface showing Markdown and an interface showing footnotes
        // agree about what "the second link" is.
        LinkStyle::Inline => {
            refs.number(href);
            out.push_raw(&format!("]({href})"));
        }
        LinkStyle::Footnotes => out.push_raw(&format!(" [{}]", refs.number(href))),
        LinkStyle::Drop => {}
    }
}

/// The numbered link list that follows a flattened body.
#[derive(Default)]
struct Footnotes {
    urls: Vec<String>,
}

impl Footnotes {
    /// The number for `href`, assigning one on first sight.
    ///
    /// Deduplicated, because a newsletter linking its logo, its title, and its
    /// "read online" to the same URL should not produce three footnotes.
    fn number(&mut self, href: &str) -> usize {
        match self.urls.iter().position(|u| u == href) {
            Some(i) => i + 1,
            None => {
                self.urls.push(href.to_string());
                self.urls.len()
            }
        }
    }

    fn render(&self) -> Option<String> {
        if self.urls.is_empty() {
            return None;
        }
        let mut s = String::from("\n\nLinks:\n");
        for (i, url) in self.urls.iter().enumerate() {
            s.push_str(&format!("[{}] {}\n", i + 1, url));
        }
        Some(s.trim_end().to_string())
    }
}

/// Schemes that can execute something when followed.
///
/// §9 says HTML is sanitized to a safe subset before rendering, and a
/// `javascript:` URL surfaced as a footnote is an invitation to paste it
/// somewhere that will run it. Dropping the href leaves the link text, which
/// is what the sender wrote and all we can vouch for.
fn is_dangerous_url(href: &str) -> bool {
    let lower = href.trim_start().to_ascii_lowercase();
    let lower = lower.replace(['\t', '\n', '\r'], "");
    lower.starts_with("javascript:") || lower.starts_with("vbscript:") || lower.starts_with("data:")
}

enum ListKind {
    Unordered,
    Ordered(usize),
}

/// The walker's output, which knows about lines.
///
/// It exists for one reason: a quoted block has to carry its marker on *every*
/// line. Writing into a bare `String` and prefixing afterwards cannot work —
/// by then the quote depth that produced each line is gone — and the previous
/// converter, which pushed `> ` once when the `<blockquote>` opened, marked
/// only the first line of every quote. In a reply chain, which is where mail
/// HTML uses `<blockquote>` at all, that is the difference between a
/// conversation and a wall of text.
struct Out {
    buf: String,
    /// Prepended to each new line: the quote markers, currently.
    prefix: String,
    /// Characters written on the current line *after* its prefix. This is what
    /// "the line is empty" means; the buffer cannot tell an empty line from one
    /// holding only a quote marker.
    line_len: usize,
    /// Completed empty lines at the tail, so `ensure_block` can be called by
    /// three different tags in a row without producing three blank lines.
    trailing_blanks: usize,
}

impl Out {
    fn new() -> Out {
        Out {
            buf: String::new(),
            prefix: String::new(),
            line_len: 0,
            trailing_blanks: 0,
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn since(&self, from: usize) -> &str {
        self.buf.get(from..).unwrap_or("")
    }

    fn truncate(&mut self, to: usize) {
        if to <= self.buf.len() {
            self.buf.truncate(to);
            self.line_len = self.buf.len() - self.buf.rfind('\n').map_or(0, |i| i + 1);
        }
    }

    fn set_quote(&mut self, depth: usize) {
        self.prefix = "> ".repeat(depth);
    }

    /// Text as written, for `<pre>`.
    fn push_verbatim(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.newline();
            } else {
                self.write(c);
            }
        }
    }

    /// Text with HTML's whitespace collapsing.
    fn push_inline(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_whitespace() {
                // A space at the start of a line is what HTML's collapsing
                // throws away, and keeping it would indent every paragraph
                // that happened to have a newline after its `<p>`.
                if self.line_len > 0 && !self.buf.ends_with(' ') {
                    self.write(' ');
                }
            } else {
                self.write(c);
            }
        }
    }

    /// Markers — `**`, `- `, `[1]` — which are not subject to collapsing.
    fn push_raw(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.newline();
            } else {
                self.write(c);
            }
        }
    }

    /// A single space between table cells, and never a leading one.
    fn separate(&mut self) {
        if self.line_len > 0 && !self.buf.ends_with(' ') {
            self.write(' ');
        }
    }

    fn write(&mut self, c: char) {
        if self.line_len == 0 && !self.prefix.is_empty() {
            let prefix = std::mem::take(&mut self.prefix);
            self.buf.push_str(&prefix);
            self.prefix = prefix;
        }
        self.buf.push(c);
        self.line_len += 1;
        self.trailing_blanks = 0;
    }

    fn newline(&mut self) {
        self.trim_trailing_spaces();
        if self.line_len == 0 {
            self.trailing_blanks += 1;
        }
        self.buf.push('\n');
        self.line_len = 0;
    }

    /// Ends the current line if anything is on it. Used where a line break is
    /// wanted but a blank line is not — between list items, say.
    fn end_line(&mut self) {
        if self.line_len > 0 {
            self.newline();
        }
    }

    /// Ends the current block: one blank line, never two.
    fn ensure_block(&mut self) {
        if self.buf.is_empty() && self.line_len == 0 {
            return;
        }
        self.end_line();
        if self.trailing_blanks == 0 {
            self.newline();
        }
    }

    fn trim_trailing_spaces(&mut self) {
        while self.buf.ends_with(' ') || self.buf.ends_with('\t') {
            self.buf.pop();
            self.line_len = self.line_len.saturating_sub(1);
        }
    }

    /// Collapses the runs of blank lines the block handling leaves behind.
    fn finish(self) -> String {
        let mut result = String::with_capacity(self.buf.len());
        let mut blanks = 0;
        for line in self.buf.lines() {
            let trimmed = line.trim_end();
            // A line holding nothing but quote markers is a blank line; it is
            // the sender's paragraph break inside a quote, not content.
            let empty = trimmed.trim_start_matches("> ").trim().is_empty();
            if empty {
                blanks += 1;
                if blanks > 1 {
                    continue;
                }
                result.push('\n');
                continue;
            }
            blanks = 0;
            result.push_str(trimmed);
            result.push('\n');
        }
        result.trim().to_string()
    }
}

enum Token {
    Text(String),
    Open(String, std::collections::HashMap<String, String>),
    Close(String),
}

/// Elements whose contents are dropped along with their tags.
const SKIP_CONTENT: &[&str] = &[
    "script", "style", "head", "title", "textarea", "noscript", "svg", "math", "template",
];

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

        // §9 again: this content is never rendered, so skip past the closing
        // tag entirely rather than emitting its text. Dropping only the tag
        // would promote a stylesheet, or the document title, to prose — which
        // is exactly how a sanitized mail ends up displaying its own CSS.
        if SKIP_CONTENT.contains(&name.as_str()) {
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
/// The HTML 4 named character references, sorted for binary search.
///
/// Not the HTML5 set, which is some 2200 entries including a long tail of
/// mathematical symbols no mail has ever contained. This is the Latin-1
/// block, Greek, and the punctuation and currency marks — which is what
/// real mail uses, and what a European sender's `&ouml;` or a receipt's
/// `&euro;` needs to stop rendering as literal `&ouml;`.
const NAMED_ENTITIES: &[(&str, char)] = &[
    ("AElig", '\u{c6}'),
    ("Aacute", '\u{c1}'),
    ("Acirc", '\u{c2}'),
    ("Agrave", '\u{c0}'),
    ("Alpha", '\u{391}'),
    ("Aring", '\u{c5}'),
    ("Atilde", '\u{c3}'),
    ("Auml", '\u{c4}'),
    ("Beta", '\u{392}'),
    ("Ccedil", '\u{c7}'),
    ("Chi", '\u{3a7}'),
    ("Dagger", '\u{2021}'),
    ("Delta", '\u{394}'),
    ("ETH", '\u{d0}'),
    ("Eacute", '\u{c9}'),
    ("Ecirc", '\u{ca}'),
    ("Egrave", '\u{c8}'),
    ("Epsilon", '\u{395}'),
    ("Eta", '\u{397}'),
    ("Euml", '\u{cb}'),
    ("Gamma", '\u{393}'),
    ("Iacute", '\u{cd}'),
    ("Icirc", '\u{ce}'),
    ("Igrave", '\u{cc}'),
    ("Iota", '\u{399}'),
    ("Iuml", '\u{cf}'),
    ("Kappa", '\u{39a}'),
    ("Lambda", '\u{39b}'),
    ("Mu", '\u{39c}'),
    ("Ntilde", '\u{d1}'),
    ("Nu", '\u{39d}'),
    ("OElig", '\u{152}'),
    ("Oacute", '\u{d3}'),
    ("Ocirc", '\u{d4}'),
    ("Ograve", '\u{d2}'),
    ("Omega", '\u{3a9}'),
    ("Omicron", '\u{39f}'),
    ("Oslash", '\u{d8}'),
    ("Otilde", '\u{d5}'),
    ("Ouml", '\u{d6}'),
    ("Phi", '\u{3a6}'),
    ("Pi", '\u{3a0}'),
    ("Prime", '\u{2033}'),
    ("Psi", '\u{3a8}'),
    ("Rho", '\u{3a1}'),
    ("Scaron", '\u{160}'),
    ("Sigma", '\u{3a3}'),
    ("THORN", '\u{de}'),
    ("Tau", '\u{3a4}'),
    ("Theta", '\u{398}'),
    ("Uacute", '\u{da}'),
    ("Ucirc", '\u{db}'),
    ("Ugrave", '\u{d9}'),
    ("Upsilon", '\u{3a5}'),
    ("Uuml", '\u{dc}'),
    ("Xi", '\u{39e}'),
    ("Yacute", '\u{dd}'),
    ("Yuml", '\u{178}'),
    ("Zeta", '\u{396}'),
    ("aacute", '\u{e1}'),
    ("acirc", '\u{e2}'),
    ("acute", '\u{b4}'),
    ("aelig", '\u{e6}'),
    ("agrave", '\u{e0}'),
    ("alefsym", '\u{2135}'),
    ("alpha", '\u{3b1}'),
    ("amp", '\u{26}'),
    ("and", '\u{2227}'),
    ("ang", '\u{2220}'),
    ("apos", '\u{27}'),
    ("aring", '\u{e5}'),
    ("asymp", '\u{2248}'),
    ("atilde", '\u{e3}'),
    ("auml", '\u{e4}'),
    ("bdquo", '\u{201e}'),
    ("beta", '\u{3b2}'),
    ("brvbar", '\u{a6}'),
    ("bull", '\u{2022}'),
    ("cap", '\u{2229}'),
    ("ccedil", '\u{e7}'),
    ("cedil", '\u{b8}'),
    ("cent", '\u{a2}'),
    ("chi", '\u{3c7}'),
    ("circ", '\u{2c6}'),
    ("clubs", '\u{2663}'),
    ("cong", '\u{2245}'),
    ("copy", '\u{a9}'),
    ("crarr", '\u{21b5}'),
    ("cup", '\u{222a}'),
    ("curren", '\u{a4}'),
    ("dArr", '\u{21d3}'),
    ("dagger", '\u{2020}'),
    ("darr", '\u{2193}'),
    ("deg", '\u{b0}'),
    ("delta", '\u{3b4}'),
    ("diams", '\u{2666}'),
    ("divide", '\u{f7}'),
    ("eacute", '\u{e9}'),
    ("ecirc", '\u{ea}'),
    ("egrave", '\u{e8}'),
    ("empty", '\u{2205}'),
    ("emsp", '\u{2003}'),
    ("ensp", '\u{2002}'),
    ("epsilon", '\u{3b5}'),
    ("equiv", '\u{2261}'),
    ("eta", '\u{3b7}'),
    ("eth", '\u{f0}'),
    ("euml", '\u{eb}'),
    ("euro", '\u{20ac}'),
    ("exist", '\u{2203}'),
    ("fnof", '\u{192}'),
    ("forall", '\u{2200}'),
    ("frac12", '\u{bd}'),
    ("frac14", '\u{bc}'),
    ("frac34", '\u{be}'),
    ("frasl", '\u{2044}'),
    ("gamma", '\u{3b3}'),
    ("ge", '\u{2265}'),
    ("gt", '\u{3e}'),
    ("hArr", '\u{21d4}'),
    ("harr", '\u{2194}'),
    ("hearts", '\u{2665}'),
    ("hellip", '\u{2026}'),
    ("iacute", '\u{ed}'),
    ("icirc", '\u{ee}'),
    ("iexcl", '\u{a1}'),
    ("igrave", '\u{ec}'),
    ("infin", '\u{221e}'),
    ("int", '\u{222b}'),
    ("iota", '\u{3b9}'),
    ("iquest", '\u{bf}'),
    ("isin", '\u{2208}'),
    ("iuml", '\u{ef}'),
    ("kappa", '\u{3ba}'),
    ("lArr", '\u{21d0}'),
    ("lambda", '\u{3bb}'),
    ("lang", '\u{2329}'),
    ("laquo", '\u{ab}'),
    ("larr", '\u{2190}'),
    ("lceil", '\u{2308}'),
    ("ldquo", '\u{201c}'),
    ("le", '\u{2264}'),
    ("lfloor", '\u{230a}'),
    ("lowast", '\u{2217}'),
    ("loz", '\u{25ca}'),
    ("lrm", '\u{200e}'),
    ("lsaquo", '\u{2039}'),
    ("lsquo", '\u{2018}'),
    ("lt", '\u{3c}'),
    ("macr", '\u{af}'),
    ("mdash", '\u{2014}'),
    ("micro", '\u{b5}'),
    ("middot", '\u{b7}'),
    ("minus", '\u{2212}'),
    ("mu", '\u{3bc}'),
    ("nabla", '\u{2207}'),
    ("nbsp", '\u{a0}'),
    ("ndash", '\u{2013}'),
    ("ne", '\u{2260}'),
    ("ni", '\u{220b}'),
    ("not", '\u{ac}'),
    ("notin", '\u{2209}'),
    ("nsub", '\u{2284}'),
    ("ntilde", '\u{f1}'),
    ("nu", '\u{3bd}'),
    ("oacute", '\u{f3}'),
    ("ocirc", '\u{f4}'),
    ("oelig", '\u{153}'),
    ("ograve", '\u{f2}'),
    ("oline", '\u{203e}'),
    ("omega", '\u{3c9}'),
    ("omicron", '\u{3bf}'),
    ("oplus", '\u{2295}'),
    ("or", '\u{2228}'),
    ("ordf", '\u{aa}'),
    ("ordm", '\u{ba}'),
    ("oslash", '\u{f8}'),
    ("otilde", '\u{f5}'),
    ("otimes", '\u{2297}'),
    ("ouml", '\u{f6}'),
    ("para", '\u{b6}'),
    ("part", '\u{2202}'),
    ("permil", '\u{2030}'),
    ("perp", '\u{22a5}'),
    ("phi", '\u{3c6}'),
    ("pi", '\u{3c0}'),
    ("piv", '\u{3d6}'),
    ("plusmn", '\u{b1}'),
    ("pound", '\u{a3}'),
    ("prime", '\u{2032}'),
    ("prod", '\u{220f}'),
    ("prop", '\u{221d}'),
    ("psi", '\u{3c8}'),
    ("quot", '\u{22}'),
    ("rArr", '\u{21d2}'),
    ("radic", '\u{221a}'),
    ("rang", '\u{232a}'),
    ("raquo", '\u{bb}'),
    ("rarr", '\u{2192}'),
    ("rceil", '\u{2309}'),
    ("rdquo", '\u{201d}'),
    ("real", '\u{211c}'),
    ("reg", '\u{ae}'),
    ("rfloor", '\u{230b}'),
    ("rho", '\u{3c1}'),
    ("rlm", '\u{200f}'),
    ("rsaquo", '\u{203a}'),
    ("rsquo", '\u{2019}'),
    ("sbquo", '\u{201a}'),
    ("scaron", '\u{161}'),
    ("sdot", '\u{22c5}'),
    ("sect", '\u{a7}'),
    ("shy", '\u{ad}'),
    ("sigma", '\u{3c3}'),
    ("sigmaf", '\u{3c2}'),
    ("sim", '\u{223c}'),
    ("spades", '\u{2660}'),
    ("sub", '\u{2282}'),
    ("sube", '\u{2286}'),
    ("sum", '\u{2211}'),
    ("sup", '\u{2283}'),
    ("sup1", '\u{b9}'),
    ("sup2", '\u{b2}'),
    ("sup3", '\u{b3}'),
    ("supe", '\u{2287}'),
    ("szlig", '\u{df}'),
    ("tau", '\u{3c4}'),
    ("there4", '\u{2234}'),
    ("theta", '\u{3b8}'),
    ("thetasym", '\u{3d1}'),
    ("thinsp", '\u{2009}'),
    ("thorn", '\u{fe}'),
    ("tilde", '\u{2dc}'),
    ("times", '\u{d7}'),
    ("trade", '\u{2122}'),
    ("uArr", '\u{21d1}'),
    ("uacute", '\u{fa}'),
    ("uarr", '\u{2191}'),
    ("ucirc", '\u{fb}'),
    ("ugrave", '\u{f9}'),
    ("uml", '\u{a8}'),
    ("upsih", '\u{3d2}'),
    ("upsilon", '\u{3c5}'),
    ("uuml", '\u{fc}'),
    ("weierp", '\u{2118}'),
    ("xi", '\u{3be}'),
    ("yacute", '\u{fd}'),
    ("yen", '\u{a5}'),
    ("yuml", '\u{ff}'),
    ("zeta", '\u{3b6}'),
    ("zwj", '\u{200d}'),
    ("zwnj", '\u{200c}'),
];

/// The named entities this build knows, in table order. For the test that the
/// table is sorted — it is binary-searched, and an unsorted entry would stop
/// decoding without any other symptom.
pub fn entity_names() -> impl Iterator<Item = &'static str> {
    NAMED_ENTITIES.iter().map(|(name, _)| *name)
}

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
            // No entity name is longer than this, and a bare `&` followed by
            // prose must not swallow the prose looking for a semicolon.
            .filter(|p| *p <= 12)
        else {
            out.push('&');
            i += 1;
            continue;
        };
        let entity: String = chars[i + 1..i + semi].iter().collect();
        let replacement = match entity.strip_prefix('#') {
            Some(number) => match number.strip_prefix(['x', 'X']) {
                Some(hexdigits) => u32::from_str_radix(hexdigits, 16).ok(),
                None => number.parse::<u32>().ok(),
            }
            .and_then(char::from_u32),
            // `&nbsp;` becomes an ordinary space rather than U+00A0. It is a
            // space to every reader, and a non-breaking one breaks `split_whitespace`,
            // which is what the preview line and the search index are built on.
            None if entity == "nbsp" => Some(' '),
            None => NAMED_ENTITIES
                .binary_search_by(|(name, _)| (*name).cmp(entity.as_str()))
                .ok()
                .map(|i| NAMED_ENTITIES[i].1),
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

/// Reduces a message's HTML to the small safe subset §9 requires.
///
/// > **Message rendering**: no remote content, no cookies, no JavaScript, no
/// > automatic external font/image loads. "Load remote content" is per-sender
/// > and remembered. HTML is sanitized to a small safe subset before
/// > rendering.
///
/// The list below is an allowlist, and that direction is the whole design. A
/// blocklist of dangerous tags is wrong every time the HTML spec grows one, and
/// it grew `<svg>` and `<math>` — both of which can carry script — years after
/// most mail sanitizers were written. Anything not named here loses its tags;
/// its *text* survives, because a mail whose content sat inside an unknown
/// element should render as prose rather than as nothing.
///
/// Remote references are not removed but disarmed: `src` becomes
/// `data-blocked-src`, so nothing loads, and a UI that later offers "load
/// remote content for this sender" still has the URL to restore. Removing them
/// outright would make that promise unkeepable.
pub fn sanitize_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut open: Vec<String> = Vec::new();

    for token in tokenize(html) {
        match token {
            // Text is escaped on the way out. It arrived as markup we already
            // decided not to trust, and re-emitting it verbatim would let a
            // half-parsed tag we skipped reassemble into a real one.
            Token::Text(t) => out.push_str(&escape_html(&decode_entities(&t))),
            Token::Open(tag, attrs) => {
                if DROPPED_WHOLE.contains(&tag.as_str()) {
                    continue;
                }
                if !ALLOWED_TAGS.contains(&tag.as_str()) {
                    continue;
                }
                out.push('<');
                out.push_str(&tag);
                for (name, value) in safe_attrs(&tag, &attrs) {
                    out.push(' ');
                    out.push_str(&name);
                    out.push_str("=\"");
                    out.push_str(&escape_html(&value));
                    out.push('"');
                }
                if VOID_TAGS.contains(&tag.as_str()) {
                    out.push_str(" />");
                } else {
                    out.push('>');
                    open.push(tag);
                }
            }
            Token::Close(tag) => {
                if !ALLOWED_TAGS.contains(&tag.as_str()) || VOID_TAGS.contains(&tag.as_str()) {
                    continue;
                }
                // Close only what is actually open, innermost first. Mail HTML
                // is full of stray `</div>`s, and emitting one that closes an
                // element we never opened hands the surrounding document's
                // structure to the sender.
                if let Some(depth) = open.iter().rposition(|t| *t == tag) {
                    for t in open.drain(depth..).rev() {
                        out.push_str(&format!("</{t}>"));
                    }
                }
            }
        }
    }

    // Whatever the sender left open, we close. An unclosed `<blockquote>` in a
    // sanitized fragment would otherwise swallow everything a UI puts after it.
    for tag in open.iter().rev() {
        out.push_str(&format!("</{tag}>"));
    }
    out
}

/// Tags whose content is markup for a machine, never text for a reader.
/// `tokenize` already skips past `script` and `style` bodies; the rest are
/// listed so their *contents* are not silently promoted to prose.
const DROPPED_WHOLE: &[&str] = &[
    "script", "style", "head", "title", "meta", "link", "base", "iframe", "frame", "frameset",
    "object", "embed", "applet", "param", "form", "input", "button", "select", "option",
    "textarea", "svg", "math", "template", "noscript",
];

/// Everything a message is allowed to say.
const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "b",
    "big",
    "blockquote",
    "br",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "samp",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

const VOID_TAGS: &[&str] = &["br", "hr", "img", "col", "wbr"];

/// The attributes that survive, per tag.
///
/// `style` is not among them anywhere. It is the attribute most people assume
/// is inert, and it is not: `background: url(https://tracker/…)` is a remote
/// load and a read receipt, and it needs no script to fire. Mail loses its
/// colours here, and that is the trade §9 already made.
fn safe_attrs(
    tag: &str,
    attrs: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let get = |k: &str| attrs.get(k).map(|v| decode_entities(v.trim()));

    match tag {
        "a" => {
            if let Some(href) = get("href").filter(|h| !h.is_empty() && !is_dangerous_url(h)) {
                out.push(("href".into(), href));
                // A link in mail opens in a new context or not at all, and
                // `noopener` keeps the opened page from reaching back through
                // `window.opener`. `noreferrer` keeps the sender's URL — and
                // so the fact that this message was read — out of the next
                // site's logs.
                out.push(("rel".into(), "noopener noreferrer nofollow".into()));
                out.push(("target".into(), "_blank".into()));
            }
        }
        "img" => {
            if let Some(alt) = get("alt") {
                out.push(("alt".into(), alt));
            }
            match get("src") {
                // An inline part: the message carries the bytes, so nothing
                // is fetched from anywhere and no one learns the mail was
                // opened. A renderer that can resolve the part uses this.
                Some(src) if src.to_ascii_lowercase().starts_with("cid:") => {
                    out.push(("data-cid".into(), src[4..].to_string()));
                }
                Some(src) if !is_dangerous_url(&src) => {
                    out.push(("data-blocked-src".into(), src));
                }
                _ => {}
            }
        }
        _ => {}
    }

    // Structural attributes with no fetching power and real meaning for a
    // reader: a table's shape, an abbreviation's expansion.
    for name in [
        "title", "colspan", "rowspan", "start", "reversed", "dir", "lang", "datetime",
    ] {
        if let Some(v) = get(name) {
            if !v.is_empty() && v.len() < 256 {
                out.push((name.to_string(), v));
            }
        }
    }
    out
}

/// URLs written out in the text itself.
///
/// Plain-text mail has no markup to carry a destination, so the URL *is* the
/// text — and an HTML mail whose link was labelled with its own address gets no
/// footnote either. Both need finding by looking at the words.
///
/// Deliberately conservative: only `http` and `https`, and trailing sentence
/// punctuation is trimmed, because `see https://example.com/x.` ends in a full
/// stop far more often than a URL does.
pub fn bare_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;

    while let Some(at) = rest.find("http") {
        let candidate = &rest[at..];
        let scheme_ok = candidate.starts_with("http://") || candidate.starts_with("https://");
        // Step past this "http" either way, so a false start cannot loop.
        let step = at + 4;
        if !scheme_ok {
            rest = &rest[step..];
            continue;
        }

        let end = candidate
            .find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"')
            .unwrap_or(candidate.len());
        let url = candidate[..end]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"'])
            .to_string();

        // Longer than the scheme, and not already listed.
        if url.len() > 8 && !out.contains(&url) {
            out.push(url);
        }
        rest = &candidate[end.max(1)..];
    }
    out
}

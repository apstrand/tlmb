//! MIME parsing, and the renderings the contract exposes (§6.4).
//!
//! The governing rule from §14.4: a malformed message may render badly; it may
//! never crash, hang, or lose data. Everything here therefore returns a value
//! for every input — there is no "this message is too broken to parse" path,
//! because the alternative is a mailbox with a hole in it where the ugly mail
//! used to be.

use std::collections::HashMap;

use mail_parser::{Address as MpAddress, Message, MessageParser, MimeHeaders, PartType};
use mailbox_api::{Address, Attachment, Header};

use crate::id;

/// Everything the store needs from one message's bytes.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    /// `Message-ID`, normalized per §5.2, or `None` when the header was absent
    /// or unusable.
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    /// `References` in header order, normalized, duplicates removed.
    pub references: Vec<String>,
    pub subject: String,
    /// From the `Date` header. `None` when it was missing or unparseable, in
    /// which case the caller substitutes `INTERNALDATE`.
    pub date: Option<i64>,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    /// Top-level headers in the order they appeared, minus the ones broken out
    /// above.
    pub headers: Vec<Header>,
    /// The best `text/plain` alternative, decoded.
    pub text: Option<String>,
    /// The best `text/html` alternative, decoded but *not* sanitized —
    /// sanitizing is the renderer's job (§9) and depends on the target.
    pub html: Option<String>,
    /// True when `text` was flattened from HTML rather than read from a real
    /// `text/plain` part.
    ///
    /// The parser fills `text` either way, which is what a reader wants. But
    /// `--text-only` (§6.4) exists precisely to say "do not show me a
    /// flattened HTML mail", and without this flag that flag would be a no-op
    /// on the messages it was written for.
    pub text_is_derived: bool,
    pub attachments: Vec<Attachment>,
    pub has_attachment: bool,
}

impl Parsed {
    /// One line of body text for a listing. Comes from the cached body, so it
    /// is absent for a message whose body has not been fetched.
    pub fn preview(&self, max: usize) -> Option<String> {
        let body = match (&self.text, &self.html) {
            (Some(t), _) => t.clone(),
            (None, Some(h)) => mail_parser::decoders::html::html_to_text(h),
            (None, None) => return None,
        };
        let mut s = String::with_capacity(max);
        for word in body.split_whitespace() {
            if s.len() + word.len() + 1 > max {
                break;
            }
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(word);
        }
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// The text handed to FTS5. Headers included, because `from:alice` is a
    /// structured query but a bare `alice` should still find her mail (§6.2).
    pub fn fts_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.subject);
        s.push('\n');
        for group in [&self.from, &self.to, &self.cc, &self.reply_to] {
            for a in group {
                if let Some(n) = &a.name {
                    s.push_str(n);
                    s.push(' ');
                }
                s.push_str(&a.address);
                s.push('\n');
            }
        }
        for a in &self.attachments {
            if let Some(f) = &a.filename {
                s.push_str(f);
                s.push('\n');
            }
        }
        match (&self.text, &self.html) {
            (Some(t), _) => s.push_str(t),
            (None, Some(h)) => s.push_str(&mail_parser::decoders::html::html_to_text(h)),
            (None, None) => {}
        }
        s
    }
}

/// Parses a whole message.
///
/// Never fails: a body that mail-parser rejects outright still yields the
/// headers we could read, and an empty body.
pub fn parse(raw: &[u8]) -> Parsed {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return parse_headers_only(raw);
    };
    let paths = part_paths(&msg);

    let mut p = Parsed {
        message_id: msg.message_id().and_then(id::normalize_message_id),
        in_reply_to: header_ids(msg.in_reply_to()).into_iter().next(),
        references: header_ids(msg.references()),
        subject: msg.subject().unwrap_or_default().to_string(),
        date: msg.date().map(|d| d.to_timestamp()),
        from: addresses(msg.from()),
        to: addresses(msg.to()),
        cc: addresses(msg.cc()),
        bcc: addresses(msg.bcc()),
        reply_to: addresses(msg.reply_to()),
        headers: other_headers(&msg),
        ..Default::default()
    };

    // The first text and HTML alternatives. Later ones are duplicates of the
    // same content in nested `multipart/alternative`s far more often than they
    // are additional prose.
    p.text = msg.body_text(0).map(|c| c.into_owned());
    p.html = msg.body_html(0).map(|c| c.into_owned());
    p.text_is_derived = msg
        .text_body
        .first()
        .and_then(|idx| msg.parts.get(*idx as usize))
        .is_some_and(|part| matches!(part.body, PartType::Html(_)));

    for (idx, part) in msg.parts.iter().enumerate() {
        if matches!(part.body, PartType::Multipart(_)) {
            continue;
        }
        // The parts backing `text`/`html` above are the message, not
        // attachments to it.
        let is_body =
            msg.text_body.contains(&(idx as u32)) || msg.html_body.contains(&(idx as u32));
        let named = part.attachment_name().is_some();
        let inline = part
            .content_disposition()
            .is_some_and(|d| d.ctype().eq_ignore_ascii_case("inline"));
        if is_body && !named {
            continue;
        }
        let size = match &part.body {
            PartType::Text(t) | PartType::Html(t) => t.len(),
            PartType::Binary(b) | PartType::InlineBinary(b) => b.len(),
            PartType::Message(_) => (part.offset_end - part.offset_body) as usize,
            PartType::Multipart(_) => 0,
        };
        let is_attachment = !inline || named;
        p.attachments.push(Attachment {
            part: paths
                .get(&idx)
                .cloned()
                .unwrap_or_else(|| (idx + 1).to_string()),
            filename: part.attachment_name().map(str::to_string),
            mime_type: mime_type_of(part),
            size: size as u64,
            content_id: part.content_id().map(str::to_string),
            is_attachment,
        });
    }
    p.has_attachment = p.attachments.iter().any(|a| a.is_attachment);
    p
}

/// The degenerate path: mail-parser refused the message entirely. Split the
/// header block by hand so `ls` can still show a row.
fn parse_headers_only(raw: &[u8]) -> Parsed {
    let text = String::from_utf8_lossy(raw);
    let head = text.split("\r\n\r\n").next().unwrap_or(&text);
    let head = head.split("\n\n").next().unwrap_or(head);
    let mut p = Parsed::default();
    for line in unfold(head) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        match name.to_ascii_lowercase().as_str() {
            "subject" => p.subject = value.to_string(),
            "message-id" => p.message_id = id::normalize_message_id(value),
            "in-reply-to" => p.in_reply_to = id::normalize_message_id(value),
            "references" => {
                p.references = value
                    .split_whitespace()
                    .filter_map(id::normalize_message_id)
                    .collect()
            }
            _ => p.headers.push(Header {
                name: name.to_string(),
                value: value.to_string(),
            }),
        }
    }
    p
}

/// Joins RFC 5322 folded header lines back together.
fn unfold(head: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in head.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

fn mime_type_of(part: &mail_parser::MessagePart<'_>) -> String {
    match part.content_type() {
        Some(ct) => match ct.subtype() {
            Some(sub) => format!(
                "{}/{}",
                ct.ctype().to_ascii_lowercase(),
                sub.to_ascii_lowercase()
            ),
            None => ct.ctype().to_ascii_lowercase(),
        },
        None => match &part.body {
            PartType::Text(_) => "text/plain".into(),
            PartType::Html(_) => "text/html".into(),
            PartType::Message(_) => "message/rfc822".into(),
            _ => "application/octet-stream".into(),
        },
    }
}

/// IMAP part numbering (RFC 3501 §6.4.5), which is what `attach <id> <part>`
/// takes, so it has to match what the server would answer to a `FETCH BODY[2.1]`.
fn part_paths(msg: &Message<'_>) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    match &msg.parts.first().map(|p| &p.body) {
        Some(PartType::Multipart(children)) => {
            for (i, &c) in children.iter().enumerate() {
                assign(msg, c as usize, &(i + 1).to_string(), &mut out);
            }
        }
        // A single-part message is part 1 in its entirety.
        Some(_) => {
            out.insert(0, "1".to_string());
        }
        None => {}
    }
    out
}

fn assign(msg: &Message<'_>, idx: usize, path: &str, out: &mut HashMap<usize, String>) {
    out.insert(idx, path.to_string());
    if let Some(PartType::Multipart(children)) = msg.parts.get(idx).map(|p| &p.body) {
        for (i, &c) in children.iter().enumerate() {
            assign(msg, c as usize, &format!("{path}.{}", i + 1), out);
        }
    }
}

fn addresses(a: Option<&MpAddress<'_>>) -> Vec<Address> {
    let Some(a) = a else { return Vec::new() };
    a.iter()
        .filter_map(|addr| {
            let address = addr.address.as_deref()?.trim();
            if address.is_empty() {
                return None;
            }
            Some(Address {
                name: addr
                    .name
                    .as_deref()
                    .map(|n| n.trim().to_string())
                    .filter(|n| !n.is_empty()),
                address: normalize_address(address),
            })
        })
        .collect()
}

/// Lowercases the domain only. The local part is case-sensitive per RFC 5321,
/// and while almost every real server ignores that, folding it here would make
/// `from:Alice@x` and `from:alice@x` different queries against the same mail.
pub fn normalize_address(a: &str) -> String {
    match a.rsplit_once('@') {
        Some((local, domain)) => format!("{local}@{}", domain.to_ascii_lowercase()),
        None => a.to_string(),
    }
}

fn header_ids(v: &mail_parser::HeaderValue<'_>) -> Vec<String> {
    let raw: Vec<&str> = match v {
        mail_parser::HeaderValue::Text(t) => vec![t.as_ref()],
        mail_parser::HeaderValue::TextList(l) => l.iter().map(|c| c.as_ref()).collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        // A `References` header is one field holding many ids; some clients
        // separate them with whitespace, some with nothing at all.
        for token in r.split_whitespace() {
            if let Some(n) = id::normalize_message_id(token) {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Headers other than the ones already broken out into typed fields, in
/// original order.
fn other_headers(msg: &Message<'_>) -> Vec<Header> {
    const BROKEN_OUT: &[&str] = &[
        "from",
        "to",
        "cc",
        "bcc",
        "reply-to",
        "subject",
        "date",
        "message-id",
        "in-reply-to",
        "references",
    ];
    msg.headers()
        .iter()
        .filter_map(|h| {
            let name = h.name().to_string();
            if BROKEN_OUT.contains(&name.to_ascii_lowercase().as_str()) {
                return None;
            }
            let value = match &h.value {
                mail_parser::HeaderValue::Text(t) => t.to_string(),
                mail_parser::HeaderValue::TextList(l) => l.join(", "),
                mail_parser::HeaderValue::DateTime(d) => d.to_rfc3339(),
                mail_parser::HeaderValue::Address(a) => a
                    .iter()
                    .map(|x| match (&x.name, &x.address) {
                        (Some(n), Some(e)) => format!("{n} <{e}>"),
                        (None, Some(e)) => e.to_string(),
                        (Some(n), None) => n.to_string(),
                        (None, None) => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => return None,
            };
            Some(Header { name, value })
        })
        .collect()
}

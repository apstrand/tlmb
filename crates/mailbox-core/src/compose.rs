//! Building an RFC 5322 message from its parts (§7's compose, §6.1's `send`).
//!
//! Two properties govern this module.
//!
//! **Deterministic.** Every varying input — the date, the `Message-ID`, the MIME
//! boundaries — comes from the injected clock and RNG (§6.7). The same draft
//! built twice under the same clock and seed is byte-identical, which is what
//! lets `--dry-run` show *the* bytes rather than bytes like them, and what lets
//! a golden file hold a whole message.
//!
//! **Header-injection-proof.** A header value that contains a newline is not a
//! long header, it is a second header. Every value goes through
//! [`sanitize_header_value`], so a subject pasted from a hostile source cannot
//! add a `Bcc:` line. This is enforced at the one place values become bytes
//! rather than trusted to callers.

use mailbox_api::Address;

use crate::clock::{Clock, Rng, Timestamp};
use crate::error::{Error, Result};

/// One attachment to be encoded into the message.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub content: Vec<u8>,
    /// `inline` rather than `attachment`, for images a body refers to.
    pub inline: bool,
    pub content_id: Option<String>,
}

/// Everything needed to build a message.
#[derive(Debug, Clone, Default)]
pub struct Draft {
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// Recipients that appear in the SMTP envelope and in no header. Putting
    /// them in a header is the classic way to leak a mailing list.
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub subject: String,
    /// The plain-text body, always present — §7 makes plain text the default.
    pub body: String,
    /// When set, the body is also rendered as HTML into a
    /// `multipart/alternative` (§7's optional Markdown→HTML).
    pub html: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<Attachment>,
    /// Extra headers, emitted after the standard ones.
    pub headers: Vec<(String, String)>,
    /// Pinned only by tests and by `reply`, which reuses nothing; normally the
    /// builder mints one.
    pub message_id: Option<String>,
    /// Pinned by tests; otherwise the clock.
    pub date: Option<Timestamp>,
}

impl Draft {
    /// Every address the message is actually delivered to — the SMTP envelope.
    pub fn envelope_recipients(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for a in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            let addr = a.address.to_ascii_lowercase();
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
        out
    }

    pub fn total_attachment_bytes(&self) -> u64 {
        self.attachments
            .iter()
            .map(|a| a.content.len() as u64)
            .sum()
    }
}

/// Builds the message.
///
/// `domain` is used for the generated `Message-ID`. It comes from the sender's
/// address rather than from the host name: a `Message-ID` naming the laptop it
/// was written on tells every recipient something they did not need to know.
/// Splits on commas that are not inside a quoted display name.
///
/// Here rather than in the CLI because every frontend takes addresses as text
/// someone typed: the compose box in the browser, the desktop window, and
/// `--to` all mean the same thing by `Alice <a@b>, bob@c`, and two parsers
/// that agree today would not stay agreed.
pub fn split_address_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// `alice@example.com` or `Alice <alice@example.com>`.
pub fn parse_address(s: &str) -> Result<Option<Address>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let (name, addr) = match (s.rfind('<'), s.rfind('>')) {
        (Some(open), Some(close)) if close > open => (
            s[..open].trim().trim_matches('"').to_string(),
            s[open + 1..close].trim(),
        ),
        _ => (String::new(), s),
    };
    if !addr.contains('@') || addr.contains(' ') {
        return Err(Error::usage(format!("`{s}` is not an email address")));
    }
    Ok(Some(Address::new(
        Some(name).filter(|n| !n.is_empty()),
        addr.to_string(),
    )))
}

/// Turns the wire form of a draft into one that can be built.
///
/// Only the parsing and the shape; nothing here decides who the message is
/// from or whether it may be sent — [`crate::Session::enqueue_send`] fills in
/// the identity and applies §6.5's policy, and it must stay the only place
/// that does.
pub fn draft_from_spec(spec: &mailbox_api::DraftSpec) -> Result<Draft> {
    let list = |fields: &[String]| -> Result<Vec<Address>> {
        let mut out = Vec::new();
        for field in fields {
            out.extend(parse_address_list(field)?);
        }
        Ok(out)
    };

    Ok(Draft {
        from: match &spec.from {
            Some(from) => parse_address(from)?,
            None => None,
        },
        to: list(&spec.to)?,
        cc: list(&spec.cc)?,
        bcc: list(&spec.bcc)?,
        subject: spec.subject.clone(),
        body: spec.body.clone(),
        in_reply_to: spec.in_reply_to.clone(),
        references: spec.references.clone(),
        ..Default::default()
    })
}

/// The wire form of a draft, for a reply the interface is about to open.
pub fn spec_from_draft(draft: &Draft) -> mailbox_api::DraftSpec {
    let list =
        |addresses: &[Address]| -> Vec<String> { addresses.iter().map(format_address).collect() };

    mailbox_api::DraftSpec {
        from: draft.from.as_ref().map(format_address),
        to: list(&draft.to),
        cc: list(&draft.cc),
        bcc: list(&draft.bcc),
        subject: draft.subject.clone(),
        body: draft.body.clone(),
        in_reply_to: draft.in_reply_to.clone(),
        references: draft.references.clone(),
    }
}

/// `Alice <a@b>`, or the bare address when there is no name.
pub fn format_address(address: &Address) -> String {
    match &address.name {
        Some(name) if !name.trim().is_empty() => format!("{name} <{}>", address.address),
        _ => address.address.clone(),
    }
}

/// Every address in one field of typed text.
pub fn parse_address_list(text: &str) -> Result<Vec<Address>> {
    let mut out = Vec::new();
    for part in split_address_list(text) {
        if let Some(address) = parse_address(&part)? {
            out.push(address);
        }
    }
    Ok(out)
}

pub fn build(draft: &Draft, clock: &Clock, rng: &Rng, domain: &str) -> Result<Vec<u8>> {
    let from = draft
        .from
        .as_ref()
        .ok_or_else(|| Error::usage("a message needs a From address"))?;
    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        return Err(Error::usage("a message needs at least one recipient"));
    }

    let date = draft.date.unwrap_or_else(|| clock.now());
    let message_id = match &draft.message_id {
        Some(id) => id.clone(),
        None => format!("<{}@{}>", rng.hex(16), domain),
    };

    let mut head = String::new();
    push_header(&mut head, "Date", &rfc2822(date))?;
    push_header(&mut head, "Message-ID", &message_id)?;
    push_address_header(&mut head, "From", std::slice::from_ref(from))?;
    if !draft.to.is_empty() {
        push_address_header(&mut head, "To", &draft.to)?;
    }
    if !draft.cc.is_empty() {
        push_address_header(&mut head, "Cc", &draft.cc)?;
    }
    // Bcc is deliberately absent. §7's blind copy is blind because the address
    // reaches the envelope and never the bytes.
    if !draft.reply_to.is_empty() {
        push_address_header(&mut head, "Reply-To", &draft.reply_to)?;
    }
    push_header(&mut head, "Subject", &encode_header_text(&draft.subject))?;
    if let Some(irt) = &draft.in_reply_to {
        push_header(&mut head, "In-Reply-To", irt)?;
    }
    if !draft.references.is_empty() {
        push_header(&mut head, "References", &fold_references(&draft.references))?;
    }
    for (name, value) in &draft.headers {
        push_header(&mut head, name, &encode_header_text(value))?;
    }
    push_header(&mut head, "MIME-Version", "1.0")?;

    // Boundaries come from the RNG, so they are unguessable in production and
    // reproducible under MAILBOX_SEED.
    let mixed = format!("=_mb_{}", rng.hex(12));
    let alt = format!("=_mb_alt_{}", rng.hex(12));

    let mut out = Vec::new();
    match (draft.attachments.is_empty(), &draft.html) {
        (true, None) => {
            head.push_str(&text_part_headers(&draft.body));
            head.push_str("\r\n");
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(encode_text(&draft.body).as_bytes());
        }
        (true, Some(html)) => {
            head.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{alt}\"\r\n\r\n"
            ));
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(alternative_body(&draft.body, html, &alt).as_bytes());
        }
        (false, _) => {
            head.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"{mixed}\"\r\n\r\n"
            ));
            out.extend_from_slice(head.as_bytes());

            out.extend_from_slice(format!("--{mixed}\r\n").as_bytes());
            match &draft.html {
                Some(html) => {
                    out.extend_from_slice(
                        format!("Content-Type: multipart/alternative; boundary=\"{alt}\"\r\n\r\n")
                            .as_bytes(),
                    );
                    out.extend_from_slice(alternative_body(&draft.body, html, &alt).as_bytes());
                }
                None => {
                    out.extend_from_slice(text_part_headers(&draft.body).as_bytes());
                    out.extend_from_slice(b"\r\n");
                    out.extend_from_slice(encode_text(&draft.body).as_bytes());
                }
            }

            for att in &draft.attachments {
                out.extend_from_slice(format!("\r\n--{mixed}\r\n").as_bytes());
                out.extend_from_slice(&render_attachment(att)?);
            }
            out.extend_from_slice(format!("\r\n--{mixed}--\r\n").as_bytes());
        }
    }
    Ok(out)
}

/// The `Message-ID` domain: the sender's, not the host's.
pub fn domain_for(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

fn text_part_headers(body: &str) -> String {
    format!(
        "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: {}\r\n",
        transfer_encoding(body)
    )
}

fn alternative_body(text: &str, html: &str, boundary: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("--{boundary}\r\n"));
    s.push_str(&text_part_headers(text));
    s.push_str("\r\n");
    s.push_str(&encode_text(text));
    s.push_str(&format!("\r\n--{boundary}\r\n"));
    s.push_str(&format!(
        "Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: {}\r\n\r\n",
        transfer_encoding(html)
    ));
    s.push_str(&encode_text(html));
    s.push_str(&format!("\r\n--{boundary}--\r\n"));
    s
}

/// 7bit for plain ASCII, quoted-printable otherwise.
///
/// Not base64: a quoted-printable body survives a `--dry-run` reading, and most
/// mail is mostly ASCII, so the encoded form stays legible.
fn transfer_encoding(body: &str) -> &'static str {
    if body.is_ascii() && !body.lines().any(|l| l.len() > 990) {
        "7bit"
    } else {
        "quoted-printable"
    }
}

fn encode_text(body: &str) -> String {
    let normalized = body.replace("\r\n", "\n").replace('\n', "\r\n");
    match transfer_encoding(body) {
        "7bit" => normalized,
        _ => quoted_printable(&normalized),
    }
}

/// Quoted-printable (RFC 2045 §6.7).
pub fn quoted_printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut line_len = 0usize;

    let push = |out: &mut String, line_len: &mut usize, chunk: &str| {
        // Soft line breaks keep every line under the 76-character limit.
        if *line_len + chunk.len() > 75 {
            out.push_str("=\r\n");
            *line_len = 0;
        }
        out.push_str(chunk);
        *line_len += chunk.len();
    };

    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            out.push_str("\r\n");
            line_len = 0;
            i += 2;
            continue;
        }
        let b = bytes[i];
        // Printable ASCII except `=`, which is the escape character. Trailing
        // whitespace is encoded because a mail gateway may strip it otherwise,
        // which would change the body's bytes and break a signature.
        let literal = matches!(b, 33..=60 | 62..=126)
            || (matches!(b, b' ' | b'\t')
                && !matches!(bytes.get(i + 1), None | Some(b'\r') | Some(b'\n')));
        if literal {
            push(&mut out, &mut line_len, &(b as char).to_string());
        } else {
            push(&mut out, &mut line_len, &format!("={b:02X}"));
        }
        i += 1;
    }
    out
}

fn render_attachment(att: &Attachment) -> Result<Vec<u8>> {
    use base64::Engine;

    let filename = sanitize_header_value(&att.filename)?;
    let disposition = if att.inline { "inline" } else { "attachment" };
    let mut head = format!(
        "Content-Type: {}; name=\"{}\"\r\n\
         Content-Disposition: {disposition}; filename=\"{}\"\r\n\
         Content-Transfer-Encoding: base64\r\n",
        sanitize_header_value(&att.mime_type)?,
        filename.replace('"', "'"),
        filename.replace('"', "'"),
    );
    if let Some(cid) = &att.content_id {
        head.push_str(&format!(
            "Content-ID: <{}>\r\n",
            sanitize_header_value(cid)?
        ));
    }
    head.push_str("\r\n");

    let mut out = head.into_bytes();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&att.content);
    for chunk in encoded.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

fn push_header(out: &mut String, name: &str, value: &str) -> Result<()> {
    let name = sanitize_header_value(name)?;
    if name.contains(':') || name.trim().is_empty() {
        return Err(Error::usage(format!("`{name}` is not a header name")));
    }
    out.push_str(&name);
    out.push_str(": ");
    out.push_str(&sanitize_header_value(value)?);
    out.push_str("\r\n");
    Ok(())
}

fn push_address_header(out: &mut String, name: &str, list: &[Address]) -> Result<()> {
    let rendered: Result<Vec<String>> = list.iter().map(render_address).collect();
    push_header(out, name, &rendered?.join(", "))
}

/// `Name <local@domain>`, with the display name encoded if it is not ASCII and
/// quoted if it contains anything that would break the grammar.
fn render_address(a: &Address) -> Result<String> {
    let addr = sanitize_header_value(&a.address)?;
    if addr.is_empty() || !addr.contains('@') {
        return Err(Error::usage(format!(
            "`{}` is not an email address",
            a.address
        )));
    }
    match a.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        None => Ok(addr),
        Some(name) => {
            let name = sanitize_header_value(name)?;
            if !name.is_ascii() {
                Ok(format!("{} <{addr}>", encode_word(&name)))
            } else if name.contains([',', ';', '<', '>', '@', '"', '(', ')', ':', '\\']) {
                Ok(format!("\"{}\" <{addr}>", name.replace('"', "'")))
            } else {
                Ok(format!("{name} <{addr}>"))
            }
        }
    }
}

/// Rejects the newline that would turn one header into two.
///
/// This is the header-injection guard, and it *rejects* rather than strips: a
/// subject that silently loses part of itself is a bug the user finds later,
/// while an error is one they find now. Also rejects NUL, which no header may
/// carry and which truncates the message on some servers.
pub fn sanitize_header_value(value: &str) -> Result<String> {
    if value.contains(['\r', '\n', '\0']) {
        return Err(Error::usage(
            "a header value cannot contain a newline — that would inject a second header",
        ));
    }
    Ok(value.to_string())
}

/// RFC 2047 encoded-word, for header text that is not ASCII.
pub fn encode_header_text(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }
    encode_word(value)
}

fn encode_word(value: &str) -> String {
    use base64::Engine;
    // Split into chunks that stay inside the 75-character encoded-word limit.
    // Splitting on character boundaries rather than bytes keeps every chunk
    // independently decodable, which some clients require.
    let mut out: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for c in value.chars() {
        if chunk.len() + c.len_utf8() > 39 {
            out.push(chunk.clone());
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        out.push(chunk);
    }
    out.iter()
        .map(|c| {
            format!(
                "=?UTF-8?B?{}?=",
                base64::engine::general_purpose::STANDARD.encode(c)
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n ")
}

/// Folds a `References` header, which routinely exceeds the line limit on a
/// long thread.
fn fold_references(refs: &[String]) -> String {
    let mut out = String::new();
    let mut line = 0usize;
    for r in refs {
        if line > 0 && line + r.len() + 1 > 76 {
            out.push_str("\r\n ");
            line = 1;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(r);
        line += r.len();
    }
    out
}

fn rfc2822(ts: Timestamp) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|d| {
            d.format(&time::format_description::well_known::Rfc2822)
                .ok()
        })
        .unwrap_or_else(|| "Thu, 01 Jan 1970 00:00:00 +0000".to_string())
}

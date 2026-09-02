//! Fixtures (§14.1): a declarative manifest that renders to bytes
//! deterministically.
//!
//! One manifest, three jobs: it seeds a real server for L2, it seeds a local
//! store directly for tests that need no server, and it *is* the expected
//! value. That is why assertions can name `fixture["invoice-with-pdf"]` and
//! never a server-assigned UID — a hardcoded `UID 4711` is a test that runs
//! against exactly one server, and provider divergence is what we are trying to
//! catch (§14).
//!
//! Determinism is enforced rather than hoped for: MIME boundaries are derived
//! from the message's symbolic name, dates come from the manifest, and a
//! `Message-ID` the manifest does not pin is derived from the name too. Nothing
//! here reads a clock or an RNG.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::clock::parse_timestamp;
use crate::error::{Error, Result};
use crate::hex;
use crate::store::ingest::Incoming;
use crate::store::{FolderRecord, Store};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// Folders to create before any message is filed. Optional: a folder named
    /// by a message is created implicitly.
    #[serde(default)]
    pub folder: Vec<FolderSpec>,
    #[serde(default)]
    pub message: Vec<MessageSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderSpec {
    pub path: String,
    /// `inbox`, `sent`, `archive`, … . Guessed from the path when absent.
    #[serde(default)]
    pub special_use: Option<String>,
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageSpec {
    /// The symbolic name assertions use. Must be unique within a manifest.
    pub name: String,
    #[serde(default = "default_folder")]
    pub folder: String,

    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub reply_to: Vec<String>,
    #[serde(default)]
    pub subject: String,

    /// RFC3339 or unix seconds. Also the `INTERNALDATE` unless `internaldate`
    /// overrides it.
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub internaldate: Option<String>,

    /// Pinned when the test cares about threading; derived from `name`
    /// otherwise, so it is stable across runs without being written out.
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,

    /// IMAP flags as the server spells them: `\Seen`, `\Flagged`, `$Junk`.
    #[serde(default)]
    pub flags: Vec<String>,

    #[serde(default)]
    pub body: String,
    /// When present, the message becomes `multipart/alternative`.
    #[serde(default)]
    pub html: Option<String>,

    /// Extra headers, emitted in order after the standard ones.
    #[serde(default)]
    pub headers: Vec<(String, String)>,

    #[serde(default)]
    pub attachment: Vec<AttachmentSpec>,

    /// Emit the message exactly as given, ignoring every other field.
    ///
    /// The escape hatch for the broken-message corpus (§14.4): some regressions
    /// can only be expressed as literal bytes, because what makes them
    /// interesting is precisely that no correct generator would produce them.
    #[serde(default)]
    pub raw: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentSpec {
    pub filename: String,
    #[serde(default = "default_mime")]
    pub mime_type: String,
    /// Literal text content.
    #[serde(default)]
    pub text: Option<String>,
    /// Base64 content, for binary attachments.
    #[serde(default)]
    pub base64: Option<String>,
    /// Deterministic filler of `n` bytes, for size-dependent tests
    /// (`larger:5M`) that should not carry five megabytes in git.
    #[serde(default)]
    pub generate_bytes: Option<usize>,
}

/// A rendered message: its bytes and everything a test needs to assert on.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub name: String,
    pub folder: String,
    pub flags: Vec<String>,
    pub internaldate: i64,
    pub message_id: String,
    pub bytes: Vec<u8>,
}

impl Rendered {
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
}

impl Fixture {
    pub fn load(path: &Path) -> Result<Fixture> {
        let text = std::fs::read_to_string(path)?;
        Fixture::parse(&text).map_err(|e| Error::usage(format!("{}: {e}", path.display())))
    }

    pub fn parse(text: &str) -> Result<Fixture> {
        let f: Fixture = toml::from_str(text).map_err(|e| Error::usage(format!("fixture: {e}")))?;
        let mut seen = std::collections::HashSet::new();
        for m in &f.message {
            if !seen.insert(&m.name) {
                return Err(Error::usage(format!(
                    "fixture has two messages named `{}`; names are how tests refer to them",
                    m.name
                )));
            }
        }
        Ok(f)
    }

    /// Renders every message, in manifest order.
    pub fn render_all(&self) -> Result<Vec<Rendered>> {
        self.message.iter().map(|m| self.render(m)).collect()
    }

    pub fn get(&self, name: &str) -> Result<&MessageSpec> {
        self.message
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| Error::not_found("fixture message", name))
    }

    /// Renders one message to RFC5322 bytes.
    pub fn render(&self, spec: &MessageSpec) -> Result<Rendered> {
        let message_id = spec
            .message_id
            .clone()
            .unwrap_or_else(|| format!("<{}@fixture.mailbox.invalid>", stable_token(&spec.name)));
        let date = spec
            .date
            .as_deref()
            .and_then(parse_timestamp)
            // A fixed epoch rather than "now": a manifest that omits a date must
            // still render identically tomorrow.
            .unwrap_or(DEFAULT_DATE);
        let internaldate = spec
            .internaldate
            .as_deref()
            .and_then(parse_timestamp)
            .unwrap_or(date);

        let bytes = match &spec.raw {
            Some(raw) => raw.replace('\n', "\r\n").into_bytes(),
            None => self.render_structured(spec, &message_id, date)?,
        };

        Ok(Rendered {
            name: spec.name.clone(),
            folder: spec.folder.clone(),
            flags: spec.flags.clone(),
            internaldate,
            message_id,
            bytes,
        })
    }

    fn render_structured(
        &self,
        spec: &MessageSpec,
        message_id: &str,
        date: i64,
    ) -> Result<Vec<u8>> {
        let mut head = String::new();
        push_header(&mut head, "Date", &rfc2822(date));
        push_header(&mut head, "Message-ID", message_id);
        if !spec.from.is_empty() {
            push_header(&mut head, "From", &spec.from);
        }
        if !spec.to.is_empty() {
            push_header(&mut head, "To", &spec.to.join(", "));
        }
        if !spec.cc.is_empty() {
            push_header(&mut head, "Cc", &spec.cc.join(", "));
        }
        if !spec.reply_to.is_empty() {
            push_header(&mut head, "Reply-To", &spec.reply_to.join(", "));
        }
        push_header(&mut head, "Subject", &spec.subject);
        if let Some(irt) = &spec.in_reply_to {
            push_header(&mut head, "In-Reply-To", irt);
        }
        if !spec.references.is_empty() {
            push_header(&mut head, "References", &spec.references.join(" "));
        }
        for (name, value) in &spec.headers {
            push_header(&mut head, name, value);
        }
        head.push_str("MIME-Version: 1.0\r\n");

        // Boundaries are derived from the message name, so re-rendering
        // produces byte-identical output and a golden file can hold the whole
        // message rather than a hash of it.
        let boundary = format!("=_mailbox_{}", stable_token(&spec.name));
        let alt_boundary = format!("{boundary}_alt");

        let mut out = Vec::new();
        let has_attachments = !spec.attachment.is_empty();
        let has_alternative = spec.html.is_some();

        if !has_attachments && !has_alternative {
            head.push_str("Content-Type: text/plain; charset=utf-8\r\n");
            head.push_str("Content-Transfer-Encoding: 8bit\r\n\r\n");
            out.extend_from_slice(head.as_bytes());
            out.extend_from_slice(crlf(&spec.body).as_bytes());
            return Ok(out);
        }

        if has_attachments {
            head.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"
            ));
        } else {
            head.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{alt_boundary}\"\r\n\r\n"
            ));
        }
        out.extend_from_slice(head.as_bytes());

        let body_part = render_body_part(spec, &alt_boundary);
        if has_attachments {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            out.extend_from_slice(body_part.as_bytes());
            for att in &spec.attachment {
                out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
                out.extend_from_slice(&render_attachment(att)?);
            }
            out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        } else {
            out.extend_from_slice(body_part.as_bytes());
        }
        Ok(out)
    }

    /// Seeds a store directly, with no server in the loop.
    ///
    /// Returns the symbolic name to stable-id map, which is what an assertion
    /// dereferences. UIDs are assigned in manifest order starting at 1 — they
    /// are ours here, so they may as well be predictable, and nothing asserts
    /// on them anyway.
    pub fn seed_store(&self, store: &mut Store) -> Result<BTreeMap<String, String>> {
        let mut folder_ids: BTreeMap<String, i64> = BTreeMap::new();

        for f in &self.folder {
            let id = store.upsert_folder(&folder_record(
                &f.path,
                f.special_use.clone(),
                &f.delimiter,
            ))?;
            folder_ids.insert(f.path.clone(), id);
        }

        let mut next_uid: BTreeMap<String, u32> = BTreeMap::new();
        let mut batch: Vec<(String, Incoming)> = Vec::new();

        for spec in &self.message {
            let rendered = self.render(spec)?;
            let folder_id = match folder_ids.get(&rendered.folder) {
                Some(id) => *id,
                None => {
                    let id = store.upsert_folder(&folder_record(&rendered.folder, None, "/"))?;
                    folder_ids.insert(rendered.folder.clone(), id);
                    id
                }
            };
            let uid = next_uid.entry(rendered.folder.clone()).or_insert(0);
            *uid += 1;

            batch.push((
                rendered.name.clone(),
                Incoming {
                    folder_id,
                    uid: *uid,
                    flags: rendered.flags.clone(),
                    internaldate: rendered.internaldate,
                    size: rendered.size(),
                    modseq: None,
                    headers: rendered.bytes.clone(),
                    // Fixtures carry whole messages, so the body is present from
                    // the start — a store seeded this way is in the state a
                    // store reaches after a body fetch, not before one.
                    raw: Some(rendered.bytes.clone()),
                },
            ));
        }

        let incoming: Vec<Incoming> = batch.iter().map(|(_, i)| i.clone()).collect();
        let applied = store.apply_batch(&incoming)?;

        let mut ids = BTreeMap::new();
        for ((name, incoming), a) in batch.iter().zip(&applied) {
            // Attach the blob too, so `show` finds the body where a real sync
            // would have put it.
            if let Some(raw) = &incoming.raw {
                store.set_body(&a.id, raw)?;
            }
            ids.insert(name.clone(), a.id.clone());
        }
        store.refresh_folder_counts()?;
        Ok(ids)
    }
}

/// 1 August 2026, 10:00:00 UTC. An arbitrary but *fixed* instant, so a manifest
/// that omits a date renders the same next year.
const DEFAULT_DATE: i64 = 1_785_060_000;

fn folder_record(path: &str, special_use: Option<String>, delimiter: &str) -> FolderRecord {
    let name = path.rsplit(delimiter).next().unwrap_or(path).to_string();
    FolderRecord {
        special_use: special_use.or_else(|| crate::imap::guess_special_use(path)),
        path: path.to_string(),
        name,
        delimiter: Some(delimiter.to_string()),
        selectable: true,
        subscribed: true,
    }
}

fn render_body_part(spec: &MessageSpec, alt_boundary: &str) -> String {
    match &spec.html {
        None => format!(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n",
            crlf(&spec.body)
        ),
        Some(html) => {
            let mut s = format!(
                "Content-Type: multipart/alternative; boundary=\"{alt_boundary}\"\r\n\r\n"
            );
            s.push_str(&format!("--{alt_boundary}\r\n"));
            s.push_str("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n");
            s.push_str(&crlf(&spec.body));
            s.push_str("\r\n");
            s.push_str(&format!("--{alt_boundary}\r\n"));
            s.push_str("Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n");
            s.push_str(&crlf(html));
            s.push_str("\r\n");
            s.push_str(&format!("--{alt_boundary}--\r\n"));
            s
        }
    }
}

fn render_attachment(att: &AttachmentSpec) -> Result<Vec<u8>> {
    use base64::Engine;
    let data: Vec<u8> = match (&att.text, &att.base64, att.generate_bytes) {
        (Some(t), _, _) => t.as_bytes().to_vec(),
        (_, Some(b64), _) => base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| Error::usage(format!("attachment {}: bad base64: {e}", att.filename)))?,
        (_, _, Some(n)) => generate_bytes(&att.filename, n),
        _ => Vec::new(),
    };

    let mut out = Vec::new();
    out.extend_from_slice(
        format!(
            "Content-Type: {}; name=\"{}\"\r\n\
             Content-Disposition: attachment; filename=\"{}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n",
            att.mime_type, att.filename, att.filename
        )
        .as_bytes(),
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    for chunk in encoded.as_bytes().chunks(76) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\r\n");
    }
    Ok(out)
}

/// Deterministic filler, so a "5 MB attachment" test does not put 5 MB in git.
fn generate_bytes(seed: &str, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed.as_bytes());
    let mut xof = hasher.finalize_xof();
    let mut buf = [0u8; 4096];
    while out.len() < n {
        xof.fill(&mut buf);
        let take = buf.len().min(n - out.len());
        out.extend_from_slice(&buf[..take]);
    }
    out
}

/// A short, stable token derived from a name — used for boundaries and for
/// generated `Message-ID`s.
fn stable_token(name: &str) -> String {
    hex::encode(&blake3::hash(name.as_bytes()).as_bytes()[..8])
}

fn push_header(out: &mut String, name: &str, value: &str) {
    out.push_str(name);
    out.push_str(": ");
    out.push_str(&value.replace(['\r', '\n'], " "));
    out.push_str("\r\n");
}

/// Normalizes to CRLF. A manifest is written with plain newlines; a message on
/// the wire is not.
fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

fn rfc2822(ts: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|d| {
            d.format(&time::format_description::well_known::Rfc2822)
                .ok()
        })
        .unwrap_or_else(|| "Thu, 01 Jan 1970 00:00:00 +0000".to_string())
}

fn default_folder() -> String {
    "INBOX".to_string()
}
fn default_delimiter() -> String {
    "/".to_string()
}
fn default_mime() -> String {
    "application/octet-stream".to_string()
}

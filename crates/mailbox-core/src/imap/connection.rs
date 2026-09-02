//! One IMAP connection: framing, TLS, authentication, and the trace hooks.
//!
//! Written rather than pulled in, for one reason: §6.7 requires *every* command
//! and response to reach the trace, with credentials redacted and literals
//! summarized. A client library that owns the socket cannot offer that, and a
//! trace with holes in it is not a trace a bug report can be diagnosed from.
//!
//! Blocking I/O throughout. §4.1 makes exactly one process per account the sync
//! engine and §5.3 bounds the idling pool, so the concurrency this needs is a
//! handful of threads — an async runtime would cost binary size (§10) to
//! schedule work that is almost entirely waiting on one socket at a time.

use std::collections::BTreeSet;
use std::io::{BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use imap_proto::{Response, Status};

use crate::error::{Error, Result};
use crate::trace::{event, redact_command, summarize_response, Tracer};

/// How much of a response line reaches the trace before it is summarized.
const TRACE_LINE_LIMIT: usize = 512;

/// Refuses to grow the parse buffer past this. A server that sends a literal
/// larger than any real message is either broken or hostile, and the recovery
/// path for both is the same: drop the connection (§14.2's L3 scenarios).
const MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    /// `Implicit` for 993, `StartTls` for 143.
    pub security: Security,
    /// §9: TLS is required by default. Set false only for a loopback host the
    /// user explicitly opted in for.
    pub require_tls: bool,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte (port 993).
    Implicit,
    /// Plaintext, then `STARTTLS` (port 143).
    StartTls,
    /// No TLS at all. Refused unless `require_tls` is false.
    None,
}

#[derive(Debug, Clone)]
pub enum Credential {
    Password(String),
    /// An OAuth2 access token, used with `XOAUTH2` (§9).
    OAuth2(String),
}

/// Either a plain TCP stream or one wrapped in TLS.
///
/// An enum rather than `Box<dyn ReadWrite>` because `STARTTLS` has to take the
/// socket back out to upgrade it, which a trait object will not let us do.
enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
    /// Held only during the `STARTTLS` upgrade.
    Upgrading,
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
            Transport::Upgrading => Ok(0),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
            Transport::Upgrading => Ok(0),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
            Transport::Upgrading => Ok(()),
        }
    }
}

pub struct Connection {
    transport: Transport,
    buf: Vec<u8>,
    tag: u64,
    tracer: Tracer,
    host: String,
    pub capabilities: BTreeSet<String>,
    /// Set once `ENABLE QRESYNC` has been accepted.
    pub qresync_enabled: bool,
}

/// Everything a command produced: the untagged responses, and how it ended.
pub struct CommandResult {
    pub responses: Vec<Response<'static>>,
    pub status: Status,
    pub code: Option<imap_proto::ResponseCode<'static>>,
    pub information: Option<String>,
}

impl CommandResult {
    pub fn ok(&self) -> bool {
        self.status == Status::Ok
    }

    /// Turns a `NO`/`BAD` into the right typed error.
    ///
    /// The distinction matters: `NO` on a `LOGIN` is an auth failure the user
    /// can fix, `BAD` is a protocol error they cannot, and §6.6 gives them
    /// different exit codes.
    pub fn require_ok(self, what: &str) -> Result<CommandResult> {
        match self.status {
            Status::Ok => Ok(self),
            Status::No => {
                let msg = self.information.clone().unwrap_or_default();
                Err(Error::Network(format!(
                    "{what} refused by the server: {msg}"
                )))
            }
            _ => {
                let msg = self.information.clone().unwrap_or_default();
                Err(Error::protocol(format!("{what}: {msg}")))
            }
        }
    }
}

impl Connection {
    /// Opens a connection and reads the greeting.
    pub fn connect(cfg: &ConnectionConfig, tracer: Tracer) -> Result<Connection> {
        if cfg.security == Security::None && cfg.require_tls {
            return Err(Error::usage(format!(
                "refusing to talk to {} in plaintext; set require_tls = false to allow it",
                cfg.host
            )));
        }

        tracer.emit(
            event::IMAP_CONNECT,
            serde_json::json!({ "host": cfg.host, "port": cfg.port, "security": format!("{:?}", cfg.security) }),
        );

        let addr = format!("{}:{}", cfg.host, cfg.port);
        let tcp = TcpStream::connect(&addr)
            .map_err(|e| Error::Network(format!("connecting to {addr}: {e}")))?;
        tcp.set_read_timeout(Some(cfg.timeout))?;
        tcp.set_write_timeout(Some(cfg.timeout))?;
        // Commands are small and latency-sensitive; Nagle would add a round
        // trip to every one of them.
        let _ = tcp.set_nodelay(true);

        let transport = match cfg.security {
            Security::Implicit => Transport::Tls(Box::new(tls_stream(&cfg.host, tcp)?)),
            Security::StartTls | Security::None => Transport::Plain(tcp),
        };

        let mut conn = Connection {
            transport,
            buf: Vec::with_capacity(16 * 1024),
            tag: 0,
            tracer,
            host: cfg.host.clone(),
            capabilities: BTreeSet::new(),
            qresync_enabled: false,
        };

        // The greeting. `BYE` here means the server refused us outright — too
        // many connections, most often (§4.1's whole premise).
        let greeting = conn.read_response()?;
        if let Response::Data {
            status: Status::Bye,
            information,
            ..
        } = &greeting
        {
            return Err(Error::Network(format!(
                "server said goodbye immediately: {}",
                information.clone().unwrap_or_default()
            )));
        }
        conn.absorb_capabilities(std::slice::from_ref(&greeting));

        if conn.capabilities.is_empty() {
            conn.capability()?;
        }
        if cfg.security == Security::StartTls {
            conn.starttls(&cfg.host)?;
        }
        Ok(conn)
    }

    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.contains(&cap.to_ascii_uppercase())
    }

    pub fn capability(&mut self) -> Result<()> {
        let result = self.command("CAPABILITY")?.require_ok("CAPABILITY")?;
        self.absorb_capabilities(&result.responses);
        Ok(())
    }

    fn starttls(&mut self, host: &str) -> Result<()> {
        if !self.has_capability("STARTTLS") {
            return Err(Error::Network(format!(
                "{host} does not offer STARTTLS, and plaintext is refused"
            )));
        }
        self.command("STARTTLS")?.require_ok("STARTTLS")?;

        let tcp = match std::mem::replace(&mut self.transport, Transport::Upgrading) {
            Transport::Plain(s) => s,
            other => {
                self.transport = other;
                return Err(Error::protocol(
                    "STARTTLS on an already-encrypted connection",
                ));
            }
        };
        self.transport = Transport::Tls(Box::new(tls_stream(host, tcp)?));

        // Capabilities before STARTTLS are not trustworthy — an active attacker
        // could have stripped or added them — so they are discarded and asked
        // for again over TLS.
        self.capabilities.clear();
        self.capability()
    }

    pub fn authenticate(&mut self, user: &str, credential: &Credential) -> Result<()> {
        if self.has_capability("LOGINDISABLED") && matches!(credential, Credential::Password(_)) {
            return Err(Error::Auth(
                "the server has disabled password login on this connection".into(),
            ));
        }

        let result = match credential {
            Credential::OAuth2(token) => {
                let payload = format!("user={user}\x01auth=Bearer {token}\x01\x01");
                self.authenticate_sasl("XOAUTH2", payload.as_bytes())?
            }
            Credential::Password(pw) if self.has_capability("AUTH=PLAIN") => {
                let payload = format!("\0{user}\0{pw}");
                self.authenticate_sasl("PLAIN", payload.as_bytes())?
            }
            Credential::Password(pw) => self.command(&format!(
                "LOGIN {} {}",
                quote_astring(user),
                quote_astring(pw)
            ))?,
        };

        if !result.ok() {
            return Err(Error::Auth(
                result
                    .information
                    .unwrap_or_else(|| "the server rejected the credential".into()),
            ));
        }
        self.absorb_capabilities(&result.responses);
        // Post-authentication capabilities differ on most servers — this is
        // where CONDSTORE and QRESYNC usually appear.
        if !self.has_capability("CONDSTORE") && !self.has_capability("IMAP4REV2") {
            self.capability()?;
        }
        Ok(())
    }

    fn authenticate_sasl(&mut self, mech: &str, payload: &[u8]) -> Result<CommandResult> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        // Send the mechanism, wait for the continuation, then the payload.
        // Not SASL-IR: not every server advertises it, and the extra round trip
        // happens once per connection.
        let tag = self.send(&format!("AUTHENTICATE {mech}"))?;
        let mut responses = Vec::new();
        loop {
            let resp = self.read_response()?;
            match resp {
                Response::Continue { .. } => {
                    self.send_raw(&encoded, true)?;
                }
                Response::Done {
                    tag: got,
                    status,
                    code,
                    information,
                } => {
                    if got.0 != tag {
                        return Err(Error::protocol("server answered a tag we did not send"));
                    }
                    return Ok(CommandResult {
                        responses,
                        status,
                        code,
                        information: information.map(|s| s.to_string()),
                    });
                }
                other => responses.push(other),
            }
        }
    }

    /// Sends a command and collects everything up to its tagged completion.
    pub fn command(&mut self, cmd: &str) -> Result<CommandResult> {
        let tag = self.send(cmd)?;
        self.collect(tag)
    }

    /// `ENABLE` (RFC 5161), for `QRESYNC` and `CONDSTORE`.
    pub fn enable(&mut self, extensions: &[&str]) -> Result<()> {
        if !self.has_capability("ENABLE") || extensions.is_empty() {
            return Ok(());
        }
        let result = self.command(&format!("ENABLE {}", extensions.join(" ")))?;
        if result.ok() && extensions.iter().any(|e| e.eq_ignore_ascii_case("QRESYNC")) {
            self.qresync_enabled = true;
        }
        Ok(())
    }

    pub fn logout(&mut self) -> Result<()> {
        let _ = self.command("LOGOUT");
        self.tracer
            .emit(event::IMAP_DISCONNECT, serde_json::Value::Null);
        Ok(())
    }

    fn send(&mut self, cmd: &str) -> Result<String> {
        self.tag += 1;
        let tag = format!("a{:04}", self.tag);
        self.send_raw(&format!("{tag} {cmd}"), false)?;
        Ok(tag)
    }

    /// Writes one CRLF-terminated line. `is_secret` suppresses the payload in
    /// the trace — used for the base64 blob after `AUTHENTICATE`.
    fn send_raw(&mut self, line: &str, is_secret: bool) -> Result<()> {
        self.tracer.emit1(
            event::IMAP_SEND,
            "line",
            if is_secret {
                "<redacted>".to_string()
            } else {
                redact_command(line)
            },
        );
        let mut w = BufWriter::new(&mut self.transport);
        w.write_all(line.as_bytes())?;
        w.write_all(b"\r\n")?;
        w.flush()?;
        Ok(())
    }

    fn collect(&mut self, tag: String) -> Result<CommandResult> {
        let mut responses = Vec::new();
        loop {
            match self.read_response()? {
                Response::Done {
                    tag: got,
                    status,
                    code,
                    information,
                } => {
                    if got.0 != tag {
                        // Out-of-order tags mean we have lost sync with the
                        // stream; continuing would attribute one command's
                        // result to another.
                        return Err(Error::protocol(format!(
                            "expected tag {tag}, server sent {}",
                            got.0
                        )));
                    }
                    return Ok(CommandResult {
                        responses,
                        status,
                        code,
                        information: information.map(|s| s.to_string()),
                    });
                }
                // An unsolicited BYE ends the session whatever we were doing.
                Response::Data {
                    status: Status::Bye,
                    information,
                    ..
                } => {
                    return Err(Error::Network(format!(
                        "server closed the session: {}",
                        information.unwrap_or_default()
                    )));
                }
                other => responses.push(other),
            }
        }
    }

    /// Reads one complete response, growing the buffer until it parses.
    ///
    /// Literals are handled by the parser rather than by counting bytes here:
    /// nom reports `Incomplete` until the whole literal has arrived, which is
    /// exactly the "literal count disagrees with its payload" case L3 injects.
    fn read_response(&mut self) -> Result<Response<'static>> {
        loop {
            match self.try_parse() {
                Parsed::Response(consumed, resp) => {
                    self.trace_response(&resp, consumed);
                    self.buf.drain(..consumed);
                    return Ok(resp);
                }
                Parsed::Unreadable => {
                    if let Some(resp) = self.recover_line()? {
                        return Ok(resp);
                    }
                    // An untagged line we could not read: dropped, and the loop
                    // goes on. Losing an untagged response degrades a sync;
                    // losing framing ends the session.
                    continue;
                }
                Parsed::NeedMore => {}
            }

            if self.buf.len() > MAX_RESPONSE_BYTES {
                return Err(Error::protocol(
                    "response exceeded the size we are willing to buffer",
                ));
            }

            let mut chunk = [0u8; 16 * 1024];
            let n = self
                .transport
                .read(&mut chunk)
                .map_err(|e| match e.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                        Error::Timeout(0)
                    }
                    _ => Error::Network(format!("reading from {}: {e}", self.host)),
                })?;
            if n == 0 {
                return Err(Error::Network(format!(
                    "{} closed the connection mid-response",
                    self.host
                )));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn try_parse(&self) -> Parsed {
        if self.buf.is_empty() {
            return Parsed::NeedMore;
        }
        match imap_proto::parser::parse_response(&self.buf) {
            Ok((rest, resp)) => Parsed::Response(self.buf.len() - rest.len(), resp.into_owned()),
            Err(nom::Err::Incomplete(_)) => Parsed::NeedMore,
            // A complete line the parser rejects is recoverable; an incomplete
            // one is not yet a line at all.
            Err(_) => match find_crlf(&self.buf) {
                Some(_) => Parsed::Unreadable,
                None => Parsed::NeedMore,
            },
        }
    }

    /// Consumes one unparseable line and salvages what it can.
    ///
    /// A line the parser cannot read must not poison the connection. Dovecot
    /// echoes a mailbox name into its error text, so a folder called `Ärchive`
    /// produces a perfectly ordinary `NO` that happens to carry 8-bit text —
    /// and giving up there would strand every later command waiting for a tag
    /// that has already gone past.
    ///
    /// Returns the salvaged response for a tagged completion or a continuation,
    /// and `None` for an untagged line, which is dropped.
    fn recover_line(&mut self) -> Result<Option<Response<'static>>> {
        let Some(end) = find_crlf(&self.buf) else {
            return Ok(None);
        };
        let line = String::from_utf8_lossy(&self.buf[..end]).into_owned();
        self.buf.drain(..end + 2);

        self.tracer.emit(
            event::IMAP_RECV,
            serde_json::json!({
                "unparseable": summarize_response(&line, TRACE_LINE_LIMIT),
                "recovered": true,
            }),
        );
        Ok(salvage_line(&line))
    }

    fn trace_response(&self, resp: &Response<'static>, bytes: usize) {
        if !self.tracer.is_enabled() {
            return;
        }
        // §6.7: bodies stay out of the trace unless --trace-bodies. Rendering
        // the parsed response rather than the raw bytes is what makes that
        // enforceable — there is no path by which a literal reaches the file.
        let rendered = if self.tracer.bodies() {
            format!("{resp:?}")
        } else {
            summarize_response(&describe(resp), TRACE_LINE_LIMIT)
        };
        self.tracer.emit(
            event::IMAP_RECV,
            serde_json::json!({ "bytes": bytes, "response": rendered }),
        );
    }

    fn absorb_capabilities(&mut self, responses: &[Response<'static>]) {
        for r in responses {
            let caps = match r {
                Response::Capabilities(caps) => Some(caps),
                Response::Data {
                    code: Some(imap_proto::ResponseCode::Capabilities(caps)),
                    ..
                } => Some(caps),
                _ => None,
            };
            let Some(caps) = caps else { continue };
            for c in caps {
                let name = match c {
                    imap_proto::Capability::Imap4rev1 => "IMAP4REV1".to_string(),
                    imap_proto::Capability::Auth(a) => format!("AUTH={}", a.to_ascii_uppercase()),
                    imap_proto::Capability::Atom(a) => a.to_ascii_uppercase(),
                };
                self.capabilities.insert(name);
            }
        }
    }
}

enum Parsed {
    Response(usize, Response<'static>),
    /// A complete line the parser rejected. Recoverable.
    Unreadable,
    NeedMore,
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Reconstructs a tagged completion or a continuation from a line the parser
/// rejected.
///
/// Only the ASCII prefix is trusted — the tag and the status word, which are
/// grammar and cannot legally be 8-bit. Everything after them is carried as
/// opaque information text.
pub fn salvage_line(line: &str) -> Option<Response<'static>> {
    if let Some(rest) = line.strip_prefix("+ ").or(line.strip_prefix("+")) {
        return Some(Response::Continue {
            code: None,
            information: Some(std::borrow::Cow::Owned(rest.trim().to_string())),
        });
    }

    let mut parts = line.splitn(3, ' ');
    let tag = parts.next()?;
    let status = parts.next()?;
    let information = parts.next().unwrap_or("").trim().to_string();

    // Untagged lines are dropped: we cannot know what they meant, and inventing
    // a meaning is worse than missing one.
    if tag.is_empty() || tag == "*" || !tag.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let status = match status.to_ascii_uppercase().as_str() {
        "OK" => Status::Ok,
        "NO" => Status::No,
        "BAD" => Status::Bad,
        "BYE" => Status::Bye,
        _ => return None,
    };
    Some(Response::Done {
        tag: imap_proto::RequestId(tag.to_string()),
        status,
        code: None,
        information: Some(std::borrow::Cow::Owned(information)),
    })
}

/// A one-line description of a response for the trace: enough to follow the
/// conversation, never enough to leak a body.
fn describe(resp: &Response<'static>) -> String {
    match resp {
        Response::Fetch(seq, attrs) => {
            let names: Vec<&str> = attrs
                .iter()
                .map(|a| match a {
                    imap_proto::AttributeValue::BodySection { .. } => "BODY[]",
                    imap_proto::AttributeValue::BodyStructure(_) => "BODYSTRUCTURE",
                    imap_proto::AttributeValue::Envelope(_) => "ENVELOPE",
                    imap_proto::AttributeValue::Flags(_) => "FLAGS",
                    imap_proto::AttributeValue::InternalDate(_) => "INTERNALDATE",
                    imap_proto::AttributeValue::ModSeq(_) => "MODSEQ",
                    imap_proto::AttributeValue::Rfc822(_) => "RFC822",
                    imap_proto::AttributeValue::Rfc822Header(_) => "RFC822.HEADER",
                    imap_proto::AttributeValue::Rfc822Size(_) => "RFC822.SIZE",
                    imap_proto::AttributeValue::Rfc822Text(_) => "RFC822.TEXT",
                    imap_proto::AttributeValue::Uid(_) => "UID",
                    imap_proto::AttributeValue::GmailLabels(_) => "X-GM-LABELS",
                    imap_proto::AttributeValue::GmailMsgId(_) => "X-GM-MSGID",
                    imap_proto::AttributeValue::GmailThrId(_) => "X-GM-THRID",
                    _ => "?",
                })
                .collect();
            format!("* {seq} FETCH ({})", names.join(" "))
        }
        Response::Data {
            status,
            information,
            ..
        } => {
            format!("* {status:?} {}", information.clone().unwrap_or_default())
        }
        other => format!("{other:?}"),
    }
}

/// Quotes an IMAP astring. Everything the user supplies goes through here:
/// a password containing a `"` or a `\` would otherwise desynchronize the
/// command stream.
pub fn quote_astring(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn tls_stream(
    host: &str,
    tcp: TcpStream,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    // §9: system roots, TLS 1.2 and up. rustls has no TLS 1.0/1.1 to disable.
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for cert in loaded.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(Error::Network(
            "no system trust roots were readable, so no certificate can be verified".into(),
        ));
    }

    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| Error::usage(format!("`{host}` is not a valid server name")))?;
    let client = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| Error::Network(format!("starting TLS with {host}: {e}")))?;
    Ok(rustls::StreamOwned::new(client, tcp))
}

impl Connection {
    /// Sends a command whose last argument is a literal.
    ///
    /// IMAP literals are a two-step handshake: the command announces `{n}`, the
    /// server answers `+`, and only then may the bytes follow. `LITERAL+`
    /// removes the round trip, and is used when advertised.
    ///
    /// This exists for `APPEND`, which the fixture seeder needs (§14.1) and
    /// which M2's `Sent` upload will need.
    pub fn append_literal(&mut self, prefix: &str, body: &[u8]) -> Result<CommandResult> {
        let synchronizing = !self.has_capability("LITERAL+") && !self.has_capability("LITERAL-");
        let marker = if synchronizing { "}" } else { "+}" };

        self.tag += 1;
        let tag = format!("a{:04}", self.tag);
        let line = format!("{tag} {prefix}{{{}{marker}", body.len());
        self.send_raw(&line, false)?;

        if synchronizing {
            // Wait for the `+` continuation. A `NO` here — over quota, folder
            // missing — arrives instead, and must be reported rather than
            // followed by a literal the server is not expecting.
            match self.read_response()? {
                Response::Continue { .. } => {}
                Response::Done {
                    status,
                    information,
                    code,
                    ..
                } => {
                    return Ok(CommandResult {
                        responses: Vec::new(),
                        status,
                        code,
                        information: information.map(|s| s.to_string()),
                    });
                }
                other => {
                    return Err(Error::protocol(format!(
                        "expected a literal continuation, got {other:?}"
                    )))
                }
            }
        }

        self.tracer.emit1(
            event::IMAP_SEND,
            "line",
            format!("<literal {} bytes>", body.len()),
        );
        self.transport.write_all(body)?;
        self.transport.write_all(b"\r\n")?;
        self.transport.flush()?;

        self.collect(tag)
    }
}

//! The SMTP submission conversation.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::imap::Security;
use crate::smtp::SendOutcome;
use crate::trace::{event, Tracer};

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    /// `StartTls` for 587 (the submission port), `Implicit` for 465.
    pub security: Security,
    /// §9: TLS required by default; false only for a loopback host the user
    /// opted in for.
    pub require_tls: bool,
    pub timeout: Duration,
    /// The name given in `EHLO`. Not the host name: §2 forbids telling a server
    /// anything it did not need, and the local host name is exactly that.
    pub ehlo_name: String,
}

enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
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

/// One SMTP reply: its code and the joined text of its lines.
#[derive(Debug, Clone)]
pub struct Reply {
    pub code: u16,
    pub text: String,
}

impl Reply {
    pub fn is_positive(&self) -> bool {
        (200..400).contains(&self.code)
    }
    pub fn is_transient(&self) -> bool {
        (400..500).contains(&self.code)
    }
}

pub struct SmtpClient {
    transport: BufReader<Transport>,
    tracer: Tracer,
    host: String,
    /// Everything the server advertised in its `EHLO` response, uppercased.
    pub extensions: Vec<String>,
}

impl SmtpClient {
    pub fn connect(cfg: &SmtpConfig, tracer: Tracer) -> Result<SmtpClient> {
        if cfg.security == Security::None && cfg.require_tls {
            return Err(Error::usage(format!(
                "refusing to submit mail to {} in plaintext; set require_tls = false to allow it",
                cfg.host
            )));
        }
        tracer.emit(
            event::SMTP_CONNECT,
            serde_json::json!({ "host": cfg.host, "port": cfg.port }),
        );

        let addr = format!("{}:{}", cfg.host, cfg.port);
        let tcp = TcpStream::connect(&addr)
            .map_err(|e| Error::Network(format!("connecting to {addr}: {e}")))?;
        tcp.set_read_timeout(Some(cfg.timeout))?;
        tcp.set_write_timeout(Some(cfg.timeout))?;
        let _ = tcp.set_nodelay(true);

        let transport = match cfg.security {
            Security::Implicit => Transport::Tls(Box::new(tls_stream(&cfg.host, tcp)?)),
            _ => Transport::Plain(tcp),
        };

        let mut client = SmtpClient {
            transport: BufReader::new(transport),
            tracer,
            host: cfg.host.clone(),
            extensions: Vec::new(),
        };

        let greeting = client.read_reply()?;
        if !greeting.is_positive() {
            return Err(Error::Network(format!(
                "{} refused the connection: {} {}",
                cfg.host, greeting.code, greeting.text
            )));
        }

        client.ehlo(&cfg.ehlo_name)?;
        if cfg.security == Security::StartTls {
            client.starttls(cfg)?;
        }
        Ok(client)
    }

    pub fn has(&self, extension: &str) -> bool {
        let wanted = extension.to_ascii_uppercase();
        self.extensions
            .iter()
            .any(|e| e == &wanted || e.starts_with(&format!("{wanted} ")))
    }

    fn ehlo(&mut self, name: &str) -> Result<()> {
        let reply = self.command(&format!("EHLO {name}"))?;
        if !reply.is_positive() {
            // A server too old for EHLO cannot do AUTH or STARTTLS either, so
            // there is nothing to fall back to for submission.
            return Err(Error::Network(format!(
                "{} rejected EHLO: {} {}",
                self.host, reply.code, reply.text
            )));
        }
        self.extensions = reply
            .text
            .lines()
            .skip(1)
            .map(|l| l.trim().to_ascii_uppercase())
            .filter(|l| !l.is_empty())
            .collect();
        Ok(())
    }

    fn starttls(&mut self, cfg: &SmtpConfig) -> Result<()> {
        if !self.has("STARTTLS") {
            return Err(Error::Network(format!(
                "{} does not offer STARTTLS, and plaintext submission is refused",
                cfg.host
            )));
        }
        let reply = self.command("STARTTLS")?;
        if !reply.is_positive() {
            return Err(Error::Network(format!(
                "{} refused STARTTLS: {} {}",
                cfg.host, reply.code, reply.text
            )));
        }

        let tcp = match std::mem::replace(self.transport.get_mut(), Transport::Upgrading) {
            Transport::Plain(s) => s,
            other => {
                *self.transport.get_mut() = other;
                return Err(Error::protocol(
                    "STARTTLS on an already-encrypted connection",
                ));
            }
        };
        // A fresh BufReader: whatever the old one had buffered was read before
        // the handshake and must not be mistaken for TLS-protected data.
        self.transport = BufReader::new(Transport::Tls(Box::new(tls_stream(&cfg.host, tcp)?)));

        // Extensions advertised before STARTTLS are not trustworthy — an active
        // attacker could have added or stripped one — so they are discarded and
        // asked for again inside the tunnel.
        self.extensions.clear();
        self.ehlo(&cfg.ehlo_name)
    }

    pub fn authenticate(&mut self, user: &str, credential: &crate::imap::Credential) -> Result<()> {
        use crate::imap::Credential;
        use base64::Engine;

        let reply = match credential {
            Credential::OAuth2(token) => {
                let payload = format!("user={user}\x01auth=Bearer {token}\x01\x01");
                self.command_secret(&format!(
                    "AUTH XOAUTH2 {}",
                    base64::engine::general_purpose::STANDARD.encode(payload)
                ))?
            }
            Credential::Password(pw) => {
                let payload = format!("\0{user}\0{pw}");
                self.command_secret(&format!(
                    "AUTH PLAIN {}",
                    base64::engine::general_purpose::STANDARD.encode(payload)
                ))?
            }
        };

        if !reply.is_positive() {
            return Err(Error::Auth(format!("{} {}", reply.code, reply.text)));
        }
        Ok(())
    }

    /// Submits one message.
    ///
    /// Never returns `Err` for a server refusal — those are [`SendOutcome`], so
    /// the caller cannot accidentally treat "the server said no" and "the
    /// socket died" the same way. `Err` here means the conversation could not
    /// be had at all.
    pub fn send(&mut self, from: &str, to: &[String], body: &[u8]) -> Result<SendOutcome> {
        if to.is_empty() {
            return Err(Error::usage("a message needs at least one recipient"));
        }

        // SIZE (RFC 1870): asking first turns "552 too big" after a megabyte
        // upload into an immediate refusal.
        let size_hint = if self.has("SIZE") {
            format!(" SIZE={}", body.len())
        } else {
            String::new()
        };

        let reply = self.command(&format!("MAIL FROM:<{from}>{size_hint}"))?;
        if !reply.is_positive() {
            return Ok(self.classify(reply, "MAIL FROM"));
        }

        // Every recipient must be accepted. A partial send — some recipients
        // accepted, some refused — would leave the user with no way to say what
        // happened, and no way to retry the rest without duplicating the first.
        for rcpt in to {
            let reply = self.command(&format!("RCPT TO:<{rcpt}>"))?;
            if !reply.is_positive() {
                let _ = self.command("RSET");
                return Ok(self.classify(reply, &format!("RCPT TO:<{rcpt}>")));
            }
        }

        let reply = self.command("DATA")?;
        // 354 is the only positive answer to DATA.
        if reply.code != 354 {
            return Ok(self.classify(reply, "DATA"));
        }

        // From here to the reply is the window §16 decided about.
        self.tracer.emit1(event::SMTP_DATA, "bytes", body.len());
        if let Err(e) = self.write_body(body) {
            // The body did not finish going out, so the server cannot have a
            // complete message. Safe to retry.
            return Ok(SendOutcome::Deferred {
                message: format!("the connection failed while sending the body: {e}"),
            });
        }

        match self.read_reply() {
            Ok(reply) if reply.is_positive() => Ok(SendOutcome::Accepted {
                response: format!("{} {}", reply.code, reply.text.trim()),
            }),
            Ok(reply) => Ok(self.classify(reply, "DATA body")),
            // The body is on the wire and the answer never came. This is the
            // double-send window (§14.3): the server may or may not have queued
            // it, and there is no way to find out.
            Err(e) => Ok(SendOutcome::Uncertain {
                message: format!("no reply after the message body: {e}"),
            }),
        }
    }

    /// 5xx is permanent, 4xx is temporary. Anything else from a server that has
    /// already answered positively once is treated as temporary, because
    /// guessing "permanent" throws mail away.
    fn classify(&self, reply: Reply, stage: &str) -> SendOutcome {
        let message = format!("{stage}: {} {}", reply.code, reply.text.trim());
        if reply.code >= 500 {
            SendOutcome::Rejected {
                code: reply.code,
                message,
            }
        } else {
            SendOutcome::Deferred { message }
        }
    }

    pub fn quit(&mut self) {
        let _ = self.command("QUIT");
        self.tracer
            .emit(event::SMTP_DISCONNECT, serde_json::Value::Null);
    }

    fn command(&mut self, line: &str) -> Result<Reply> {
        self.tracer.emit1(event::SMTP_SEND, "line", line);
        self.write_line(line)?;
        self.read_reply()
    }

    /// A command whose payload is a credential. §6.7: the trace is something a
    /// user attaches to a bug report.
    fn command_secret(&mut self, line: &str) -> Result<Reply> {
        let verb = line.split(' ').take(2).collect::<Vec<_>>().join(" ");
        self.tracer
            .emit1(event::SMTP_SEND, "line", format!("{verb} <redacted>"));
        self.write_line(line)?;
        self.read_reply()
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        let t = self.transport.get_mut();
        t.write_all(line.as_bytes())?;
        t.write_all(b"\r\n")?;
        t.flush()?;
        Ok(())
    }

    /// Writes the message body with dot-stuffing and the terminating `.`.
    fn write_body(&mut self, body: &[u8]) -> Result<()> {
        let t = self.transport.get_mut();
        let mut start = 0usize;
        let mut i = 0usize;

        while i < body.len() {
            if body[i] == b'\n' {
                let line_end = if i > 0 && body[i - 1] == b'\r' {
                    i - 1
                } else {
                    i
                };
                write_stuffed(t, &body[start..line_end])?;
                t.write_all(b"\r\n")?;
                start = i + 1;
            }
            i += 1;
        }
        if start < body.len() {
            write_stuffed(t, &body[start..])?;
            t.write_all(b"\r\n")?;
        }

        t.write_all(b".\r\n")?;
        t.flush()?;
        Ok(())
    }

    /// Reads one reply, joining the lines of a multi-line one.
    fn read_reply(&mut self) -> Result<Reply> {
        let code;
        let mut text = String::new();

        loop {
            let mut line = String::new();
            let n = self
                .transport
                .read_line(&mut line)
                .map_err(|e| Error::Network(format!("reading from {}: {e}", self.host)))?;
            if n == 0 {
                return Err(Error::Network(format!(
                    "{} closed the connection",
                    self.host
                )));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            self.tracer.emit1(event::SMTP_RECV, "line", line);

            if line.len() < 3 {
                return Err(Error::protocol(format!("short SMTP reply: {line:?}")));
            }
            let this_code: u16 = line[..3]
                .parse()
                .map_err(|_| Error::protocol(format!("SMTP reply with no code: {line:?}")))?;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line[3..].trim_start_matches([' ', '-']));

            // A hyphen after the code means another line follows.
            if line.as_bytes().get(3) != Some(&b'-') {
                code = this_code;
                break;
            }
        }
        Ok(Reply { code, text })
    }
}

/// Dot-stuffing (RFC 5321 §4.5.2): a line that begins with `.` gets another,
/// or it would end the message early.
fn write_stuffed(out: &mut impl Write, line: &[u8]) -> std::io::Result<()> {
    if line.first() == Some(&b'.') {
        out.write_all(b".")?;
    }
    out.write_all(line)
}

fn tls_stream(
    host: &str,
    tcp: TcpStream,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
    crate::tls::stream(host, tcp)
}

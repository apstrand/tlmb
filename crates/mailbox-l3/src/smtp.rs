//! An SMTP submission server that misbehaves on request.
//!
//! The fault that justifies the whole crate is [`Fault::DropBeforeFinalReply`]:
//! the server reads the whole message, keeps it, and closes the connection
//! without saying `250`. The client cannot tell that from a message that never
//! arrived. §14.3 names this the double-send window and gives it a dedicated
//! test; §16 decides what to do about it.
//!
//! Every message the server accepts is recorded, so a test can count
//! deliveries — which is the only way to prove either that a retry duplicated a
//! message or that it did not.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where in the conversation a fault fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Auth,
    MailFrom,
    Rcpt,
    /// The `DATA` command itself, before the body.
    Data,
    /// After the body's terminating `.`, where the `250` would go.
    DataEnd,
}

/// What the server should do wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum Fault {
    /// Behave.
    None,
    /// Answer `4xx` at this stage. The client should retry.
    Transient(Stage),
    /// Answer `5xx` at this stage. The client should give up.
    Permanent(Stage),
    /// **The double-send window** (§14.3): read the whole message, keep it, and
    /// close the connection without the final `250`.
    ///
    /// The message *is* delivered. The client has no way to know.
    DropBeforeFinalReply,
    /// Close the connection partway through the body. Nothing is delivered, and
    /// a retry is safe.
    DropDuringData,
    /// Wait this long before answering the given stage, for timeout paths.
    Delay(Stage, Duration),
    /// Advertise `STARTTLS` and then refuse it — the downgrade attempt §9's
    /// `require_tls` exists to survive.
    RefuseStartTls,
    /// Never advertise `STARTTLS` at all.
    NoStartTls,
}

/// One message the server accepted, as it arrived.
#[derive(Debug, Clone)]
pub struct ReceivedMessage {
    pub from: String,
    pub to: Vec<String>,
    pub body: Vec<u8>,
    /// Which connection delivered it, so a test can tell a retry from a batch.
    pub connection: u32,
}

impl ReceivedMessage {
    /// The `Message-ID` header, which is how a test identifies a delivery
    /// without depending on anything the server assigned.
    pub fn message_id(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.body);
        for line in text.lines() {
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Message-ID: ") {
                return Some(v.trim().to_string());
            }
        }
        None
    }

    pub fn subject(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.body);
        text.lines()
            .take_while(|l| !l.is_empty())
            .find_map(|l| l.strip_prefix("Subject: ").map(|v| v.trim().to_string()))
    }
}

struct Shared {
    fault: Mutex<Fault>,
    received: Mutex<Vec<ReceivedMessage>>,
    /// Set to stop the accept loop.
    stop: AtomicBool,
    /// Whether `AUTH` is required and what credential is accepted.
    credential: Mutex<Option<String>>,
    connections: Mutex<u32>,
}

/// A running server. Dropping it stops the accept loop.
pub struct SmtpServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
}

impl SmtpServer {
    /// Starts on an ephemeral loopback port.
    ///
    /// Loopback and ephemeral on purpose: a test server that binds a fixed port
    /// cannot run in parallel, and one that binds a routable address is a mail
    /// server somebody will find.
    pub fn start() -> std::io::Result<SmtpServer> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            fault: Mutex::new(Fault::None),
            received: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            credential: Mutex::new(None),
            connections: Mutex::new(0),
        });

        let worker = Arc::clone(&shared);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let session = Arc::clone(&worker);
                // One thread per connection: the client is single-threaded, but
                // a retry opens a second connection while the first may not
                // have been reaped yet.
                std::thread::spawn(move || {
                    let _ = handle(stream, session);
                });
            }
        });

        Ok(SmtpServer { addr, shared })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Arms a fault. Takes effect on the next connection.
    pub fn set_fault(&self, fault: Fault) {
        *self.shared.fault.lock().expect("fault lock") = fault;
    }

    /// Requires `AUTH`, accepting exactly this base64 `PLAIN` payload.
    pub fn require_auth(&self, payload: Option<String>) {
        *self.shared.credential.lock().expect("credential lock") = payload;
    }

    /// Every message accepted so far, in arrival order.
    pub fn received(&self) -> Vec<ReceivedMessage> {
        self.shared.received.lock().expect("received lock").clone()
    }

    /// How many messages carry this `Message-ID` — the exactly-once assertion.
    pub fn deliveries_of(&self, message_id: &str) -> usize {
        let wanted = message_id.trim_matches(['<', '>']);
        self.received()
            .iter()
            .filter(|m| {
                m.message_id()
                    .map(|id| id.trim_matches(['<', '>']) == wanted)
                    .unwrap_or(false)
            })
            .count()
    }

    pub fn clear(&self) {
        self.shared.received.lock().expect("received lock").clear();
    }
}

impl Drop for SmtpServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Unblock the accept loop so the thread can notice and exit.
        let _ = TcpStream::connect(self.addr);
    }
}

fn handle(stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    let connection = {
        let mut n = shared.connections.lock().expect("connection lock");
        *n += 1;
        *n
    };
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let fault = shared.fault.lock().expect("fault lock").clone();
    let requires_auth = shared.credential.lock().expect("credential lock").is_some();

    writeln!(out, "220 l3.mailbox.invalid ESMTP ready\r")?;
    out.flush()?;

    let mut from = String::new();
    let mut to: Vec<String> = Vec::new();
    let mut authenticated = !requires_auth;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        let upper = line.to_ascii_uppercase();

        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            writeln!(out, "250-l3.mailbox.invalid\r")?;
            writeln!(out, "250-SIZE 35882577\r")?;
            writeln!(out, "250-8BITMIME\r")?;
            if !matches!(fault, Fault::NoStartTls) {
                writeln!(out, "250-STARTTLS\r")?;
            }
            writeln!(out, "250 AUTH PLAIN LOGIN\r")?;
            out.flush()?;
            continue;
        }

        if upper.starts_with("STARTTLS") {
            // Either refusal is a downgrade: a client that then continues in
            // plaintext has failed §9, and that is what the test checks.
            writeln!(out, "454 TLS not available right now\r")?;
            out.flush()?;
            continue;
        }

        if upper.starts_with("AUTH") {
            if let Some(reply) = fault_reply(&fault, Stage::Auth) {
                writeln!(out, "{reply}\r")?;
                out.flush()?;
                continue;
            }
            let expected = shared.credential.lock().expect("credential lock").clone();
            let offered = line.split(' ').nth(2).unwrap_or("").to_string();
            match expected {
                Some(want) if want != offered => {
                    writeln!(out, "535 authentication failed\r")?;
                    out.flush()?;
                    continue;
                }
                _ => {
                    authenticated = true;
                    writeln!(out, "235 authenticated\r")?;
                    out.flush()?;
                    continue;
                }
            }
        }

        if upper.starts_with("MAIL FROM") {
            if !authenticated {
                writeln!(out, "530 authentication required\r")?;
                out.flush()?;
                continue;
            }
            delay_for(&fault, Stage::MailFrom);
            if let Some(reply) = fault_reply(&fault, Stage::MailFrom) {
                writeln!(out, "{reply}\r")?;
                out.flush()?;
                continue;
            }
            from = extract_path(&line);
            to.clear();
            writeln!(out, "250 sender ok\r")?;
            out.flush()?;
            continue;
        }

        if upper.starts_with("RCPT TO") {
            delay_for(&fault, Stage::Rcpt);
            if let Some(reply) = fault_reply(&fault, Stage::Rcpt) {
                writeln!(out, "{reply}\r")?;
                out.flush()?;
                continue;
            }
            to.push(extract_path(&line));
            writeln!(out, "250 recipient ok\r")?;
            out.flush()?;
            continue;
        }

        if upper.starts_with("DATA") {
            delay_for(&fault, Stage::Data);
            if let Some(reply) = fault_reply(&fault, Stage::Data) {
                writeln!(out, "{reply}\r")?;
                out.flush()?;
                continue;
            }
            writeln!(out, "354 go ahead\r")?;
            out.flush()?;

            let body = match read_body(&mut reader, &fault, &mut out) {
                Some(body) => body,
                // DropDuringData: the connection is gone and nothing was kept,
                // which is exactly the state a retry is safe from.
                None => return Ok(()),
            };

            delay_for(&fault, Stage::DataEnd);
            if let Some(reply) = fault_reply(&fault, Stage::DataEnd) {
                writeln!(out, "{reply}\r")?;
                out.flush()?;
                continue;
            }

            // The message is kept *before* the reply is written. That ordering
            // is the whole point of the double-send fault: with
            // DropBeforeFinalReply the message is delivered and the client is
            // never told.
            shared
                .received
                .lock()
                .expect("received lock")
                .push(ReceivedMessage {
                    from: from.clone(),
                    to: to.clone(),
                    body,
                    connection,
                });

            if matches!(fault, Fault::DropBeforeFinalReply) {
                return Ok(());
            }
            writeln!(out, "250 2.0.0 accepted\r")?;
            out.flush()?;
            continue;
        }

        if upper.starts_with("RSET") {
            from.clear();
            to.clear();
            writeln!(out, "250 reset\r")?;
            out.flush()?;
            continue;
        }

        if upper.starts_with("QUIT") {
            writeln!(out, "221 bye\r")?;
            out.flush()?;
            return Ok(());
        }

        if upper.starts_with("NOOP") {
            writeln!(out, "250 ok\r")?;
            out.flush()?;
            continue;
        }

        writeln!(out, "500 unrecognized\r")?;
        out.flush()?;
    }
}

/// Reads the dot-terminated body, un-stuffing leading dots.
///
/// Returns `None` when the connection was cut mid-body on purpose.
fn read_body(
    reader: &mut BufReader<TcpStream>,
    fault: &Fault,
    _out: &mut TcpStream,
) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut lines = 0usize;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "." {
            return Some(body);
        }

        lines += 1;
        if matches!(fault, Fault::DropDuringData) && lines > 3 {
            // Hang up partway through. Nothing is recorded, so a retry cannot
            // duplicate anything.
            return None;
        }

        let unstuffed = trimmed.strip_prefix('.').unwrap_or(trimmed);
        body.extend_from_slice(unstuffed.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
}

fn fault_reply(fault: &Fault, stage: Stage) -> Option<String> {
    match fault {
        Fault::Transient(s) if *s == stage => Some("451 4.3.0 try again later".to_string()),
        Fault::Permanent(s) if *s == stage => Some("550 5.1.1 no such mailbox".to_string()),
        _ => None,
    }
}

fn delay_for(fault: &Fault, stage: Stage) {
    if let Fault::Delay(s, d) = fault {
        if *s == stage {
            std::thread::sleep(*d);
        }
    }
}

/// Pulls the address out of `MAIL FROM:<a@b>` or `RCPT TO:<a@b>`.
fn extract_path(line: &str) -> String {
    match (line.find('<'), line.find('>')) {
        (Some(a), Some(b)) if b > a => line[a + 1..b].to_string(),
        // A bare-address form is legal enough that refusing it would make the
        // server stricter than the ones we are pretending to be.
        _ => line
            .split(':')
            .nth(1)
            .map(|s| s.split(' ').next().unwrap_or("").trim().to_string())
            .unwrap_or_default(),
    }
}

//! An IMAP server that misbehaves on request.
//!
//! §14.2 names the scenarios: truncated literals, a literal count that
//! disagrees with its payload, `UIDVALIDITY` bumped mid-session, `BYE` during a
//! `FETCH`, a `CAPABILITY` advertising `QRESYNC` that then rejects it,
//! arbitrary latency and connection drops. No real server will do any of these
//! for you, which is exactly why the recovery paths for them are the least
//! tested code in any mail client.
//!
//! It speaks enough IMAP4rev1 for our sync engine and nothing more. The point
//! is not to be a mail server; it is to be a *hostile* one, cheaply.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the server should do wrong.
#[derive(Debug, Clone, PartialEq)]
pub enum ImapFault {
    None,
    /// Announce `{N}` and then send fewer than `N` bytes before closing.
    ///
    /// The client must not hang waiting for bytes that will never come, and
    /// must not treat the partial literal as a message.
    TruncatedLiteral,
    /// Announce a literal shorter than the payload that follows, so the parser
    /// resynchronizes onto the middle of a message body.
    LiteralCountTooSmall,
    /// Change `UIDVALIDITY` between one `EXAMINE` and the next, mid-session.
    ///
    /// §5.3 promises a rescan; this is what proves the promise is kept when the
    /// change arrives in the middle of a pass rather than between them.
    BumpUidValidityAfterFirstSelect,
    /// Answer a `UID FETCH` with every message, ignoring the set asked for.
    ///
    /// Real servers do volunteer rows nobody asked for — that is how a client
    /// ever sees a UID it already holds carrying a different message. It is a
    /// fault rather than the default because a server that always did it makes
    /// a chunked fetch indistinguishable from a whole one, and a client that
    /// asked for the wrong UIDs would pass every test.
    FetchIgnoresUidSet,
    /// Send `BYE` partway through a `FETCH` response.
    ByeDuringFetch,
    /// Advertise `QRESYNC` in `CAPABILITY` and then refuse to `ENABLE` it.
    ///
    /// The client has to fall back to the slower path rather than assuming the
    /// extension it was promised.
    QresyncAdvertisedThenRejected,
    /// Advertise `QRESYNC`, enable it, then reject the `EXAMINE` that uses it.
    QresyncEnabledThenSelectRejected,
    /// Hang up after this many commands, whatever they were.
    DropAfterCommands(u32),
    /// Delay every response.
    Delay(Duration),
    /// Acknowledge `IDLE` and then never say anything, so the client's own
    /// window is the only thing that ends the wait.
    ///
    /// This is the case RFC 2177 exists for: a server that is perfectly happy
    /// to hold an idle connection open forever, and a client that must
    /// re-issue anyway because the network between them will not.
    SilentIdle,
    /// Acknowledge `IDLE` and then hang up without a `BYE`, mid-wait.
    DropDuringIdle,
}

/// One message the server serves.
#[derive(Debug, Clone)]
pub struct Message {
    pub uid: u32,
    /// IMAP flag atoms, e.g. `\Seen`.
    pub flags: Vec<String>,
    pub internaldate: String,
    pub body: Vec<u8>,
}

impl Message {
    /// A minimal RFC5322 message with the given subject and `Message-ID`.
    pub fn simple(uid: u32, subject: &str, message_id: &str) -> Message {
        let body = format!(
            "Date: Tue, 01 Sep 2026 12:00:00 +0000\r\n\
             From: Alice <alice@example.com>\r\n\
             To: peter@example.com\r\n\
             Subject: {subject}\r\n\
             Message-ID: <{message_id}>\r\n\
             \r\n\
             Body of {subject}.\r\n"
        )
        .into_bytes();
        Message {
            uid,
            flags: Vec::new(),
            internaldate: "01-Sep-2026 12:00:00 +0000".to_string(),
            body,
        }
    }

    fn header_block(&self) -> Vec<u8> {
        let text = String::from_utf8_lossy(&self.body);
        let head = text.split("\r\n\r\n").next().unwrap_or(&text);
        format!("{head}\r\n\r\n").into_bytes()
    }
}

struct Shared {
    fault: Mutex<ImapFault>,
    /// How long an `IDLE` waits before announcing new mail. `None` waits for
    /// `DONE`.
    idle_wake: Mutex<Option<Duration>>,
    /// Whether `IDLE` appears in `CAPABILITY`. Plenty of servers do not offer
    /// it, and the loop has to work against those too.
    offers_idle: AtomicBool,
    messages: Mutex<Vec<Message>>,
    uidvalidity: AtomicU32,
    /// How many times a mailbox has been selected, for the mid-session bump.
    selects: AtomicU32,
    commands: AtomicU32,
    stop: AtomicBool,
}

pub struct ImapServer {
    addr: SocketAddr,
    shared: Arc<Shared>,
}

impl ImapServer {
    /// Starts on an ephemeral loopback port, serving `messages` in `INBOX`.
    pub fn start(messages: Vec<Message>) -> std::io::Result<ImapServer> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            fault: Mutex::new(ImapFault::None),
            idle_wake: Mutex::new(None),
            offers_idle: AtomicBool::new(true),
            messages: Mutex::new(messages),
            uidvalidity: AtomicU32::new(1000),
            selects: AtomicU32::new(0),
            commands: AtomicU32::new(0),
            stop: AtomicBool::new(false),
        });

        let worker = Arc::clone(&shared);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if worker.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let session = Arc::clone(&worker);
                std::thread::spawn(move || {
                    let _ = serve(stream, session);
                });
            }
        });

        Ok(ImapServer { addr, shared })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn set_fault(&self, fault: ImapFault) {
        *self.shared.fault.lock().expect("fault lock") = fault;
    }

    /// Stops advertising `IDLE`, so the client has to fall back to polling.
    pub fn advertise_idle(&self, offers: bool) {
        self.shared.offers_idle.store(offers, Ordering::SeqCst);
    }

    /// Makes an `IDLE` announce new mail after `delay` instead of waiting for
    /// `DONE`. This is how the background loop is woken in a test without
    /// anything sleeping for minutes.
    pub fn wake_idle_after(&self, delay: Option<Duration>) {
        *self.shared.idle_wake.lock().expect("idle lock") = delay;
    }

    pub fn uidvalidity(&self) -> u32 {
        self.shared.uidvalidity.load(Ordering::SeqCst)
    }

    /// Replaces the served messages, the way another client would.
    pub fn set_messages(&self, messages: Vec<Message>) {
        *self.shared.messages.lock().expect("messages lock") = messages;
    }

    /// Resets the per-session counters so a fault keyed on "the first select"
    /// fires again for the next test phase.
    pub fn reset_counters(&self) {
        self.shared.selects.store(0, Ordering::SeqCst);
        self.shared.commands.store(0, Ordering::SeqCst);
    }
}

impl Drop for ImapServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(stream: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut out = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    let fault = shared.fault.lock().expect("fault lock").clone();
    // Advertised whenever a fault is armed, so the QRESYNC-specific faults
    // have something to lie about — but not for faults about something else,
    // which would otherwise send the client down the QRESYNC path and test a
    // different thing than the one that was asked for.
    let advertises_qresync = !matches!(fault, ImapFault::None | ImapFault::FetchIgnoresUidSet);

    write!(
        out,
        "* OK [CAPABILITY IMAP4rev1 LITERAL+ AUTH=PLAIN] l3 ready\r\n"
    )?;
    out.flush()?;

    let mut selected = false;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        let mut parts = line.splitn(2, ' ');
        let tag = parts.next().unwrap_or("*").to_string();
        let rest = parts.next().unwrap_or("").to_string();
        let verb = rest.split(' ').next().unwrap_or("").to_ascii_uppercase();

        let count = shared.commands.fetch_add(1, Ordering::SeqCst) + 1;
        if let ImapFault::DropAfterCommands(n) = fault {
            if count > n {
                // Hang up without a word, the way a middlebox does.
                return Ok(());
            }
        }
        if let ImapFault::Delay(d) = fault {
            std::thread::sleep(d);
        }

        match verb.as_str() {
            "IDLE" => {
                if fault == ImapFault::DropDuringIdle {
                    write!(out, "+ idling\r\n")?;
                    out.flush()?;
                    // No BYE, no DONE, no close handshake: the way a middlebox
                    // reaps a connection nobody has spoken on.
                    return Ok(());
                }

                write!(out, "+ idling\r\n")?;
                out.flush()?;

                let wake = *shared.idle_wake.lock().expect("idle lock");
                if let Some(delay) = wake.filter(|_| fault != ImapFault::SilentIdle) {
                    std::thread::sleep(delay);
                    // The one thing a client cannot ignore: the mailbox is a
                    // different size than it was told.
                    let count = shared.messages.lock().expect("messages lock").len();
                    write!(out, "* {count} EXISTS\r\n")?;
                    out.flush()?;
                }

                // Whatever happened, the wait ends when the client says so.
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 {
                        return Ok(());
                    }
                    if line.trim().eq_ignore_ascii_case("DONE") {
                        break;
                    }
                }
                write!(out, "{tag} OK idle done\r\n")?;
            }
            "CAPABILITY" => {
                write!(
                    out,
                    "* CAPABILITY {}\r\n",
                    capabilities(
                        advertises_qresync,
                        shared.offers_idle.load(Ordering::SeqCst)
                    )
                )?;
                write!(out, "{tag} OK done\r\n")?;
            }
            "LOGIN" | "AUTHENTICATE" => {
                if verb == "AUTHENTICATE" {
                    write!(out, "+ \r\n")?;
                    out.flush()?;
                    let mut ignored = String::new();
                    reader.read_line(&mut ignored)?;
                }
                write!(
                    out,
                    "{tag} OK [CAPABILITY {}] logged in\r\n",
                    capabilities(
                        advertises_qresync,
                        shared.offers_idle.load(Ordering::SeqCst)
                    )
                )?;
            }
            "ENABLE" => {
                if matches!(fault, ImapFault::QresyncAdvertisedThenRejected)
                    && rest.to_ascii_uppercase().contains("QRESYNC")
                {
                    // Advertised in CAPABILITY, refused here. A client that
                    // assumed the extension it was promised now has a QRESYNC
                    // SELECT it must not send.
                    write!(out, "{tag} NO QRESYNC is not available\r\n")?;
                } else {
                    write!(out, "* ENABLED QRESYNC\r\n{tag} OK enabled\r\n")?;
                }
            }
            "LIST" => {
                write!(out, "* LIST () \"/\" INBOX\r\n{tag} OK done\r\n")?;
            }
            "STATUS" => {
                let n = shared.messages.lock().expect("messages").len();
                write!(
                    out,
                    "* STATUS INBOX (MESSAGES {n} UNSEEN {n})\r\n{tag} OK done\r\n"
                )?;
            }
            "EXAMINE" | "SELECT" => {
                let nth = shared.selects.fetch_add(1, Ordering::SeqCst) + 1;
                if matches!(fault, ImapFault::BumpUidValidityAfterFirstSelect) && nth > 1 {
                    // The mailbox is not the mailbox it was a moment ago.
                    shared.uidvalidity.fetch_add(1, Ordering::SeqCst);
                }
                if matches!(fault, ImapFault::QresyncEnabledThenSelectRejected)
                    && rest.to_ascii_uppercase().contains("QRESYNC")
                {
                    write!(out, "{tag} NO QRESYNC parameters rejected\r\n")?;
                    out.flush()?;
                    continue;
                }

                let messages = shared.messages.lock().expect("messages");
                let uidnext = messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1;
                write!(out, "* {} EXISTS\r\n", messages.len())?;
                write!(out, "* 0 RECENT\r\n")?;
                write!(
                    out,
                    "* OK [UIDVALIDITY {}] uids valid\r\n",
                    shared.uidvalidity.load(Ordering::SeqCst)
                )?;
                write!(out, "* OK [UIDNEXT {uidnext}] predicted\r\n")?;
                write!(out, "* OK [HIGHESTMODSEQ 1] highest\r\n")?;
                write!(out, "{tag} OK [READ-WRITE] selected\r\n")?;
                selected = true;
            }
            "UID" => {
                let sub = rest.split(' ').nth(1).unwrap_or("").to_ascii_uppercase();
                match sub.as_str() {
                    "SEARCH" => {
                        let messages = shared.messages.lock().expect("messages");
                        let uids: Vec<String> =
                            messages.iter().map(|m| m.uid.to_string()).collect();
                        write!(out, "* SEARCH {}\r\n{tag} OK done\r\n", uids.join(" "))?;
                    }
                    "FETCH" => {
                        if !selected {
                            write!(out, "{tag} BAD no mailbox selected\r\n")?;
                        } else {
                            let wants_full = rest.contains("BODY.PEEK[]");
                            // `UID FETCH <set> <items>` — the set is the third
                            // word. Honouring it is what lets a test see that
                            // a client asked for a *slice*; a server that
                            // returns everything however little was asked for
                            // makes a chunked fetch indistinguishable from a
                            // whole one.
                            let set = rest.split(' ').nth(2).unwrap_or("1:*").to_string();
                            if fetch(&mut out, &shared, &fault, wants_full, &tag, &set)? {
                                return Ok(());
                            }
                        }
                    }
                    "STORE" | "MOVE" | "COPY" | "EXPUNGE" => {
                        write!(out, "{tag} OK done\r\n")?;
                    }
                    other => {
                        write!(out, "{tag} BAD unsupported UID {other}\r\n")?;
                    }
                }
            }
            "LOGOUT" => {
                write!(out, "* BYE\r\n{tag} OK logged out\r\n")?;
                out.flush()?;
                return Ok(());
            }
            "NOOP" | "CHECK" | "CLOSE" | "UNSELECT" => {
                write!(out, "{tag} OK done\r\n")?;
            }
            _ => {
                write!(out, "{tag} BAD unsupported {verb}\r\n")?;
            }
        }
        out.flush()?;
    }
}

fn capabilities(qresync: bool, idle: bool) -> String {
    let mut caps = String::from("IMAP4rev1 LITERAL+ UIDPLUS MOVE ENABLE LIST-EXTENDED SPECIAL-USE");
    if idle {
        caps.push_str(" IDLE");
    }
    if qresync {
        // Advertised whenever a fault is armed, so the QRESYNC-specific faults
        // have something to lie about.
        caps.push_str(" CONDSTORE QRESYNC");
    }
    caps
}

/// An IMAP UID set: `5`, `3001:5000`, `1,3,7:9`, `4:*`.
#[derive(Debug, Clone)]
struct UidSet {
    ranges: Vec<(u32, u32)>,
}

impl UidSet {
    /// `highest` is what `*` means, which the server knows and the set does not.
    fn parse(set: &str, highest: u32) -> UidSet {
        let mut ranges = Vec::new();
        for part in set.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let bound = |s: &str| -> u32 {
                if s == "*" {
                    highest
                } else {
                    s.parse().unwrap_or(0)
                }
            };
            match part.split_once(':') {
                Some((lo, hi)) => {
                    let (a, b) = (bound(lo), bound(hi));
                    ranges.push((a.min(b), a.max(b)));
                }
                None => {
                    let uid = bound(part);
                    ranges.push((uid, uid));
                }
            }
        }
        // An unparseable set is everything, which is what this server did
        // before it read the set at all — a fault should have to be asked for.
        if ranges.is_empty() {
            ranges.push((1, highest.max(1)));
        }
        UidSet { ranges }
    }

    fn contains(&self, uid: u32) -> bool {
        self.ranges.iter().any(|(lo, hi)| uid >= *lo && uid <= *hi)
    }
}

/// Writes a `FETCH` response, injecting whichever literal fault is armed.
///
/// Returns true when the connection should be dropped afterwards.
fn fetch(
    out: &mut TcpStream,
    shared: &Arc<Shared>,
    fault: &ImapFault,
    wants_full: bool,
    tag: &str,
    set: &str,
) -> std::io::Result<bool> {
    let all = shared.messages.lock().expect("messages").clone();
    let highest = all.iter().map(|m| m.uid).max().unwrap_or(0);
    let wanted = UidSet::parse(set, highest);
    let messages: Vec<Message> = if *fault == ImapFault::FetchIgnoresUidSet {
        all.clone()
    } else {
        all.iter()
            .filter(|m| wanted.contains(m.uid))
            .cloned()
            .collect()
    };

    for (i, m) in messages.iter().enumerate() {
        let payload = if wants_full {
            m.body.clone()
        } else {
            m.header_block()
        };
        let section = if wants_full { "BODY[]" } else { "BODY[HEADER]" };
        let flags = m.flags.join(" ");

        match fault {
            ImapFault::TruncatedLiteral => {
                // Announce the real length, send half, hang up. The client must
                // not hang waiting for the rest, and must not keep the half.
                write!(
                    out,
                    "* {} FETCH (UID {} FLAGS ({flags}) INTERNALDATE \"{}\" RFC822.SIZE {} {section} {{{}}}\r\n",
                    i + 1,
                    m.uid,
                    m.internaldate,
                    payload.len(),
                    payload.len()
                )?;
                out.write_all(&payload[..payload.len() / 2])?;
                out.flush()?;
                return Ok(true);
            }
            ImapFault::LiteralCountTooSmall => {
                // Claim less than we send, so the parser resynchronizes onto
                // the middle of a message body and sees garbage where a
                // response should be.
                let claimed = payload.len() / 2;
                write!(
                    out,
                    "* {} FETCH (UID {} FLAGS ({flags}) INTERNALDATE \"{}\" RFC822.SIZE {} {section} {{{claimed}}}\r\n",
                    i + 1,
                    m.uid,
                    m.internaldate,
                    payload.len()
                )?;
                out.write_all(&payload)?;
                write!(out, ")\r\n")?;
                out.flush()?;
                continue;
            }
            ImapFault::ByeDuringFetch if i == messages.len().saturating_sub(1) && i > 0 => {
                // The session ends partway through the batch. Everything
                // already sent is good; the rest must be re-fetched, not lost.
                write!(out, "* BYE server going down\r\n")?;
                out.flush()?;
                return Ok(true);
            }
            _ => {}
        }

        write!(
            out,
            "* {} FETCH (UID {} FLAGS ({flags}) INTERNALDATE \"{}\" RFC822.SIZE {} {section} {{{}}}\r\n",
            i + 1,
            m.uid,
            m.internaldate,
            payload.len(),
            payload.len()
        )?;
        out.write_all(&payload)?;
        write!(out, ")\r\n")?;
    }

    write!(out, "{tag} OK fetch done\r\n")?;
    Ok(false)
}

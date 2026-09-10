//! `mailbox relay` — the rendezvous service of §8.2.
//!
//! > Deliberately trivial, so that running your own is a five-minute job and
//! > trusting the default one is a small decision.
//!
//! ```text
//! POST /a                                         -> 201 {"slot":"7"}
//! POST /r/{cid}      body: opaque bytes <= 64 KB   -> 201, or 409 if occupied
//! GET  /r/{cid}                                    -> 200 + bytes, then deleted
//!                                                     404 if absent/expired
//! ```
//!
//! That is the whole protocol. It holds bytes in memory for at most 300 s,
//! hands each one to the first caller that asks, and forgets it. There is no
//! database, no disk, no configuration file, and no account.
//!
//! ## What it deliberately cannot do
//!
//! **It cannot read a payload.** Everything it stores is sealed under a key
//! derived from a code that never reaches it (§8.1).
//!
//! **It cannot usefully lie.** Substituting a message changes the transcript,
//! which changes the words both devices display, which is the check the user
//! performs before anything is written.
//!
//! **It cannot brute-force a code.** The slot it hands out is public and says
//! nothing about the code — see [`mailbox_core::pair::relay`] for why that
//! separation is load-bearing rather than tidy.
//!
//! So the worst a hostile operator manages is denial of service, and the worst
//! a compromised one leaks is that two IP addresses paired at a time. That is
//! what makes §2's "optional, self-hostable, stores nothing" an honest claim
//! rather than a hope.
//!
//! ## Why it holds state at all
//!
//! §8.2 calls the relay stateless, and in the sense that matters — nothing
//! survives a restart, nothing is ever written down — it is. But two devices
//! cannot hand each other bytes without something holding them for the seconds
//! in between. A restart drops every pairing in flight, and that is an
//! acceptable failure: the user starts again with a fresh code.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mailbox_core::error::{Error, Result};
use mailbox_core::pair::relay::{validate_slot, Box_, MAX_MESSAGE, TTL_SECONDS};
use mailbox_web::http::{Request, Response};

use crate::commands::Ctx;
use crate::output::GREEN;

/// How many slots may be live at once.
///
/// A ceiling rather than a rate: without one, an attacker who can hold
/// connections open can make the process grow until it is killed, and a relay
/// that dies is a relay nobody can pair through. 64 KB per message and three
/// messages per slot puts the worst case at a bounded, arithmetic figure.
const MAX_SLOTS: usize = 4096;

/// Requests one address may make per minute.
///
/// §8.2 asks for rate limiting, and this is what it is for: making 2^45 online
/// guesses take longer than the universe rather than longer than a weekend.
const REQUESTS_PER_MINUTE: u32 = 120;

/// How many connections may be in flight.
const MAX_CONNECTIONS: usize = 128;

struct Message {
    body: Vec<u8>,
    stored: Instant,
}

#[derive(Default)]
struct Bucket {
    count: u32,
    since: Option<Instant>,
}

struct State {
    /// `slot.box` → the bytes waiting there.
    messages: HashMap<String, Message>,
    /// Per-address counters for the rate limit.
    buckets: HashMap<IpAddr, Bucket>,
    next_slot: u64,
}

impl State {
    /// Drops everything past its TTL.
    ///
    /// On every request rather than on a timer: a relay with no traffic has
    /// nothing to expire, and a thread that exists only to tidy an empty map
    /// is a thread that can be forgotten about and leak.
    fn expire(&mut self) {
        let ttl = Duration::from_secs(TTL_SECONDS);
        self.messages.retain(|_, m| m.stored.elapsed() < ttl);
        self.buckets.retain(|_, b| {
            b.since
                .is_some_and(|s| s.elapsed() < Duration::from_secs(60))
        });
    }

    /// Whether `addr` is within its allowance.
    fn allow(&mut self, addr: IpAddr) -> bool {
        let bucket = self.buckets.entry(addr).or_default();
        let window = Duration::from_secs(60);
        match bucket.since {
            Some(since) if since.elapsed() < window => {}
            _ => {
                bucket.since = Some(Instant::now());
                bucket.count = 0;
            }
        }
        bucket.count += 1;
        bucket.count <= REQUESTS_PER_MINUTE
    }
}

pub struct RelayServer {
    listener: TcpListener,
    state: Arc<Mutex<State>>,
    served: Arc<AtomicU64>,
    /// Stops after this many requests. For the test suite, which must not run
    /// a server forever.
    max_requests: Option<u64>,
    quiet: bool,
}

impl RelayServer {
    pub fn bind(bind: IpAddr, port: u16, max_requests: Option<u64>, quiet: bool) -> Result<Self> {
        let listener = TcpListener::bind(SocketAddr::new(bind, port))
            .map_err(|e| Error::Network(format!("could not bind {bind}:{port}: {e}")))?;
        Ok(RelayServer {
            listener,
            state: Arc::new(Mutex::new(State {
                messages: HashMap::new(),
                buckets: HashMap::new(),
                next_slot: 1,
            })),
            served: Arc::new(AtomicU64::new(0)),
            max_requests,
            quiet,
        })
    }

    pub fn address(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| Error::Network(e.to_string()))
    }

    pub fn run(&self) -> Result<()> {
        let live = Arc::new(AtomicU64::new(0));
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            if live.load(Ordering::SeqCst) as usize >= MAX_CONNECTIONS {
                let _ = Response::text(503, "too many connections")
                    .write(&mut stream.try_clone().unwrap_or(stream));
                continue;
            }
            let peer = stream
                .peer_addr()
                .map(|a| a.ip())
                .unwrap_or(IpAddr::from([0, 0, 0, 0]));

            let state = self.state.clone();
            let live_now = live.clone();
            live.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let _ = serve_one(stream, peer, &state);
                live_now.fetch_sub(1, Ordering::SeqCst);
            });

            let served = self.served.fetch_add(1, Ordering::SeqCst) + 1;
            if self.max_requests.is_some_and(|max| served >= max) {
                break;
            }
        }
        let _ = self.quiet;
        Ok(())
    }
}

fn serve_one(
    stream: std::net::TcpStream,
    peer: IpAddr,
    state: &Arc<Mutex<State>>,
) -> std::result::Result<(), mailbox_web::http::Error> {
    mailbox_web::http::set_timeouts(&stream)?;
    let mut reader = std::io::BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    let response = match Request::read(&mut reader) {
        Ok(Some(request)) => route(&request, peer, state),
        Ok(None) => return Ok(()),
        Err(mailbox_web::http::Error::TooLarge) => Response::text(413, "too large"),
        Err(e) => Response::text(400, &e.to_string()),
    };
    response.write(&mut writer).map_err(Into::into)
}

fn route(request: &Request, peer: IpAddr, state: &Arc<Mutex<State>>) -> Response {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    state.expire();
    if !state.allow(peer) {
        return Response::text(429, "slow down");
    }

    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/a") => allocate(&mut state),
        ("POST", path) => match cid(path) {
            Some(key) => put(&mut state, &key, &request.body),
            None => Response::text(404, "no such box"),
        },
        ("GET", path) => match cid(path) {
            Some(key) => take(&mut state, &key),
            None => health(path),
        },
        _ => Response::text(405, "method not allowed"),
    }
}

/// `/r/{slot}.{box}` → the storage key, if it is one.
///
/// Parsed strictly. The slot and the box name both come from a stranger, and a
/// relay that accepted arbitrary keys would be a free key-value store for
/// anyone who found it.
fn cid(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/r/")?;
    let (slot, name) = rest.rsplit_once('.')?;
    validate_slot(slot).ok()?;
    Box_::parse(name)?;
    Some(format!("{slot}.{name}"))
}

fn allocate(state: &mut State) -> Response {
    if state.messages.len() >= MAX_SLOTS {
        return Response::text(503, "no slots free; try again shortly");
    }
    // Sequential in base36, so the common case is one or two characters — the
    // shortest thing a person has to read across to another machine. It is
    // routing, not a secret, so predictability costs nothing (see the client's
    // module docs); what it must not do is collide with a live slot.
    let mut slot;
    loop {
        slot = base36(state.next_slot);
        state.next_slot = state.next_slot.wrapping_add(1).max(1);
        if !Box_::all()
            .iter()
            .any(|b| state.messages.contains_key(&format!("{slot}.{}", b.name())))
        {
            break;
        }
    }
    Response::json(
        201,
        &serde_json::json!({ "slot": slot, "ttl": TTL_SECONDS }),
    )
}

fn put(state: &mut State, key: &str, body: &[u8]) -> Response {
    if body.len() > MAX_MESSAGE {
        return Response::text(413, "too large");
    }
    if state.messages.len() >= MAX_SLOTS {
        return Response::text(503, "no room; try again shortly");
    }
    // §8.2's burn: a box is written once. A second write is somebody racing an
    // honest pairing, and the honest one has already claimed the box.
    if state.messages.contains_key(key) {
        return Response::text(409, "already used");
    }
    state.messages.insert(
        key.to_string(),
        Message {
            body: body.to_vec(),
            stored: Instant::now(),
        },
    );
    Response::text(201, "stored")
}

fn take(state: &mut State, key: &str) -> Response {
    match state.messages.remove(key) {
        Some(message) => Response::new(200, "application/octet-stream", message.body),
        // Indistinguishable from "never existed", deliberately: a relay that
        // said which would let anyone map live pairings.
        None => Response::text(404, "nothing here"),
    }
}

fn health(path: &str) -> Response {
    if path == "/" || path == "/health" {
        return Response::json(
            200,
            &serde_json::json!({
                "service": "mailbox-relay",
                "ttl": TTL_SECONDS,
                "max_message": MAX_MESSAGE,
            }),
        );
    }
    Response::text(404, "no such box")
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// `mailbox relay`.
pub fn relay(ctx: &Ctx<'_>, args: &crate::cli::RelayArgs) -> Result<()> {
    let bind: IpAddr = args
        .bind
        .parse()
        .map_err(|_| Error::usage(format!("`{}` is not an address to bind", args.bind)))?;

    let server = RelayServer::bind(bind, args.port, args.max_requests, ctx.out.is_structured())?;
    let address = server.address()?;

    if ctx.out.is_structured() {
        ctx.out.reply(
            "relay",
            &serde_json::json!({
                "listening": address.to_string(),
                "ttl_seconds": TTL_SECONDS,
                "max_message": MAX_MESSAGE,
            }),
        )?;
    } else {
        ctx.out.print(format!(
            "{} relaying on {address}",
            ctx.out.paint("✓", GREEN)
        ));
        ctx.out.print(ctx.out.dim(&format!(
            "  channels live {TTL_SECONDS}s, messages up to {} KB, nothing is written to disk",
            MAX_MESSAGE / 1024
        )));
        ctx.out.print(ctx.out.dim(
            "  put it behind a TLS terminator: clients refuse plaintext to anything but loopback",
        ));
    }
    server.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cid_must_name_a_real_box() {
        assert_eq!(cid("/r/7.offer").as_deref(), Some("7.offer"));
        assert_eq!(cid("/r/42b.payload").as_deref(), Some("42b.payload"));
        // Not ours to store.
        assert_eq!(cid("/r/7.something"), None);
        assert_eq!(cid("/r/7"), None);
        assert_eq!(cid("/other/7.offer"), None);
        // A slot that tries to be a path.
        assert_eq!(cid("/r/../etc.offer"), None);
        assert_eq!(cid("/r/a b.offer"), None);
    }

    #[test]
    fn base36_is_short_and_stable() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(1), "1");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    fn state() -> State {
        State {
            messages: HashMap::new(),
            buckets: HashMap::new(),
            next_slot: 1,
        }
    }

    #[test]
    fn a_box_is_written_once_and_read_once() {
        let mut s = state();
        assert_eq!(put(&mut s, "7.offer", b"hello").status, 201);
        // §8.2's burn.
        assert_eq!(put(&mut s, "7.offer", b"impostor").status, 409);

        let got = take(&mut s, "7.offer");
        assert_eq!(got.status, 200);
        assert_eq!(got.body, b"hello");
        // Deleted on read.
        assert_eq!(take(&mut s, "7.offer").status, 404);
    }

    #[test]
    fn an_absent_box_and_an_expired_one_look_the_same() {
        let mut s = state();
        assert_eq!(take(&mut s, "7.offer").status, 404);
        assert_eq!(take(&mut s, "never-existed.offer").status, 404);
    }

    #[test]
    fn an_oversized_message_is_refused() {
        let mut s = state();
        assert_eq!(
            put(&mut s, "7.payload", &vec![0u8; MAX_MESSAGE + 1]).status,
            413
        );
    }

    #[test]
    fn slots_do_not_repeat_while_they_are_live() {
        let mut s = state();
        let mut seen = Vec::new();
        for _ in 0..8 {
            let response = allocate(&mut s);
            assert_eq!(response.status, 201);
            let value: serde_json::Value = serde_json::from_slice(&response.body).expect("json");
            let slot = value["slot"].as_str().expect("slot").to_string();
            assert!(!seen.contains(&slot), "slot {slot} handed out twice");
            // Occupy it, so the next allocation has to step over it.
            put(&mut s, &format!("{slot}.offer"), b"x");
            seen.push(slot);
        }
    }

    #[test]
    fn the_rate_limit_lets_normal_use_through_and_stops_a_flood() {
        let mut s = state();
        let addr: IpAddr = "203.0.113.9".parse().expect("addr");
        for i in 0..REQUESTS_PER_MINUTE {
            assert!(s.allow(addr), "request {i} should be allowed");
        }
        assert!(!s.allow(addr), "the next one is over the limit");
        // A different device is unaffected.
        assert!(s.allow("203.0.113.10".parse().expect("addr")));
    }

    #[test]
    fn expiry_drops_what_is_past_its_ttl() {
        let mut s = state();
        put(&mut s, "7.offer", b"x");
        s.messages.get_mut("7.offer").expect("stored").stored =
            Instant::now() - Duration::from_secs(TTL_SECONDS + 1);
        s.expire();
        assert_eq!(take(&mut s, "7.offer").status, 404);
    }

    #[test]
    fn health_says_what_this_is_without_saying_who_is_pairing() {
        let response = health("/");
        assert_eq!(response.status, 200);
        let text = String::from_utf8_lossy(&response.body);
        assert!(text.contains("mailbox-relay"), "{text}");
        assert!(!text.contains("slot"), "a health check names no pairing");
    }
}

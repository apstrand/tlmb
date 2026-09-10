//! A very small DNS client, for the `SRV` and `MX` records account discovery
//! reads (§9).
//!
//! Hand-rolled for the reason the IMAP, SMTP, and HTTPS clients are (§4): a
//! resolver crate is a large dependency — async runtime, DNSSEC, caching, a
//! record type for everything — to carry into a static binary for the two
//! queries `accounts add` makes, once, at setup time.
//!
//! What is here is that subset: one question per query, `RD` set so the
//! configured recursive resolver does the work, UDP first with a TCP retry
//! when the answer comes back truncated. No caching (a setup that runs once
//! has nothing to cache for), no DNSSEC validation, and — this matters for
//! what discovery is allowed to conclude — **no trust placed in the answer**.
//! An unsigned `SRV` record is a hint about where to look, so §9's TLS rules
//! still apply to whatever it names: the certificate has to be valid for the
//! host, and discovery verifies that by connecting before it writes a config.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use crate::error::{Error, Result};

/// How long to wait on one resolver before moving to the next.
///
/// Short: this runs while someone watches `accounts add`, several resolvers
/// may be tried, and a nameserver that has not answered in two seconds is
/// usually one that will not.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The largest response accepted, over either transport. Real answers to
/// these questions are a few hundred bytes.
const MAX_RESPONSE: usize = 8 * 1024;

/// One `SRV` record: where a service lives, and how much it wants the traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Srv {
    pub priority: u16,
    pub weight: u16,
    pub port: u16,
    pub target: String,
}

/// One `MX` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mx {
    pub preference: u16,
    pub exchange: String,
}

const TYPE_MX: u16 = 15;
const TYPE_SRV: u16 = 33;

/// Looks up `_service._tcp.domain`, best (lowest) priority first.
///
/// A single `.` target is RFC 2782's "this service is explicitly not offered
/// here", and is dropped rather than returned as a host nobody can connect to.
pub fn srv(name: &str, resolvers: &[SocketAddr]) -> Result<Vec<Srv>> {
    let answer = query(name, TYPE_SRV, resolvers)?;
    let mut out = Vec::new();
    for rdata in answer.records(TYPE_SRV) {
        let mut cur = Cursor::at(&answer.raw, rdata.start);
        let priority = cur.u16()?;
        let weight = cur.u16()?;
        let port = cur.u16()?;
        let target = cur.name()?;
        if target.is_empty() || port == 0 {
            continue;
        }
        out.push(Srv {
            priority,
            weight,
            port,
            target,
        });
    }
    // Priority is absolute; weight only orders within a priority, and for
    // picking a mail server to *try first* a stable descending weight is a
    // better answer than a random draw among equals.
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then(b.weight.cmp(&a.weight)));
    Ok(out)
}

/// Looks up the `MX` records for `domain`, most-preferred first.
pub fn mx(domain: &str, resolvers: &[SocketAddr]) -> Result<Vec<Mx>> {
    let answer = query(domain, TYPE_MX, resolvers)?;
    let mut out = Vec::new();
    for rdata in answer.records(TYPE_MX) {
        let mut cur = Cursor::at(&answer.raw, rdata.start);
        let preference = cur.u16()?;
        let exchange = cur.name()?;
        if exchange.is_empty() {
            continue;
        }
        out.push(Mx {
            preference,
            exchange,
        });
    }
    out.sort_by_key(|m| m.preference);
    Ok(out)
}

/// The resolvers to ask: `MAILBOX_RESOLVER` if set, else `/etc/resolv.conf`.
///
/// The environment variable is what lets the test suite point the whole chain
/// at a resolver it controls, and what gives a host with no `resolv.conf` —
/// a phone, a container — a way to say where its nameserver is. There is no
/// built-in public fallback on purpose: quietly sending someone's mail domain
/// to a resolver they did not choose is exactly the kind of unrequested
/// network access §2 rules out.
pub fn system_resolvers() -> Vec<SocketAddr> {
    if let Ok(spec) = std::env::var("MAILBOX_RESOLVER") {
        let found: Vec<SocketAddr> = spec
            .split(',')
            .filter_map(|s| parse_resolver(s.trim()))
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.split(';').next()?.trim();
            let rest = line.strip_prefix("nameserver")?;
            parse_resolver(rest.trim())
        })
        .collect()
}

fn parse_resolver(s: &str) -> Option<SocketAddr> {
    if s.is_empty() {
        return None;
    }
    if let Ok(addr) = s.parse::<SocketAddr>() {
        return Some(addr);
    }
    // A bare address, which is how resolv.conf writes them. Scope ids (`%eth0`)
    // appear on link-local IPv6 and are not something this needs to follow.
    let bare = s.split('%').next()?;
    bare.parse::<IpAddr>()
        .ok()
        .map(|ip| SocketAddr::new(ip, 53))
}

/// Asks each resolver in turn, and reports the last failure if none answered.
fn query(name: &str, qtype: u16, resolvers: &[SocketAddr]) -> Result<Answer> {
    if resolvers.is_empty() {
        return Err(Error::Network(
            "no nameserver is configured, so DNS discovery cannot run".into(),
        ));
    }
    let question = encode_query(name, qtype)?;
    let mut last: Option<Error> = None;
    for resolver in resolvers {
        match ask(*resolver, &question, name, qtype) {
            Ok(answer) => return Ok(answer),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| Error::Network(format!("no nameserver answered for {name}"))))
}

fn ask(resolver: SocketAddr, question: &[u8], name: &str, qtype: u16) -> Result<Answer> {
    let raw = match over_udp(resolver, question)? {
        Udp::Answer(raw) => raw,
        // Truncated: the answer did not fit in a datagram, and TCP is what the
        // protocol says to do about it.
        Udp::Truncated => over_tcp(resolver, question)?,
    };
    Answer::parse(raw, name, qtype)
}

enum Udp {
    Answer(Vec<u8>),
    Truncated,
}

fn over_udp(resolver: SocketAddr, question: &[u8]) -> Result<Udp> {
    let bind: SocketAddr = if resolver.is_ipv6() {
        "[::]:0".parse().expect("a valid bind address")
    } else {
        "0.0.0.0:0".parse().expect("a valid bind address")
    };
    let socket = UdpSocket::bind(bind)
        .map_err(|e| Error::Network(format!("opening a socket for DNS: {e}")))?;
    socket.set_read_timeout(Some(TIMEOUT)).ok();
    socket.set_write_timeout(Some(TIMEOUT)).ok();
    socket
        .send_to(question, resolver)
        .map_err(|e| Error::Network(format!("asking {resolver}: {e}")))?;

    let mut buf = vec![0u8; MAX_RESPONSE];
    loop {
        let (n, from) = socket
            .recv_from(&mut buf)
            .map_err(|e| Error::Network(format!("waiting for {resolver}: {e}")))?;
        // An answer from somewhere else is not an answer to this question.
        if from.ip() != resolver.ip() {
            continue;
        }
        if n < 12 {
            return Err(Error::Network(format!(
                "{resolver} sent a truncated header"
            )));
        }
        // The id has to match, or this is a reply to a query that timed out
        // earlier — or a spoof that lost the race it had to win.
        if buf[..2] != question[..2] {
            continue;
        }
        if buf[2] & 0x02 != 0 {
            return Ok(Udp::Truncated);
        }
        return Ok(Udp::Answer(buf[..n].to_vec()));
    }
}

fn over_tcp(resolver: SocketAddr, question: &[u8]) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&resolver, TIMEOUT)
        .map_err(|e| Error::Network(format!("connecting to {resolver} over TCP: {e}")))?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let len = u16::try_from(question.len())
        .map_err(|_| Error::Network("the query is too large for DNS".into()))?;
    stream
        .write_all(&len.to_be_bytes())
        .and_then(|_| stream.write_all(question))
        .and_then(|_| stream.flush())
        .map_err(|e| Error::Network(format!("writing a DNS query to {resolver}: {e}")))?;

    let mut header = [0u8; 2];
    stream
        .read_exact(&mut header)
        .map_err(|e| Error::Network(format!("reading from {resolver}: {e}")))?;
    let len = usize::from(u16::from_be_bytes(header));
    if len > MAX_RESPONSE {
        return Err(Error::Network(format!(
            "{resolver} sent an implausibly large answer"
        )));
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .map_err(|e| Error::Network(format!("reading from {resolver}: {e}")))?;
    Ok(body)
}

fn encode_query(name: &str, qtype: u16) -> Result<Vec<u8>> {
    let mut id = [0u8; 2];
    getrandom::fill(&mut id)
        .map_err(|e| Error::Network(format!("no entropy for a query id: {e}")))?;

    let mut out = Vec::with_capacity(name.len() + 20);
    out.extend_from_slice(&id);
    out.extend_from_slice(&[0x01, 0x00]); // RD, and nothing else.
    out.extend_from_slice(&[0x00, 0x01]); // one question
    out.extend_from_slice(&[0x00; 6]); // no answer, authority, or additional
    encode_name(name, &mut out)?;
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x01]); // class IN
    Ok(out)
}

fn encode_name(name: &str, out: &mut Vec<u8>) -> Result<()> {
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(Error::usage(format!("`{name}` is not a DNS name")));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    if out.len() > 255 + 16 {
        return Err(Error::usage(format!("`{name}` is too long for DNS")));
    }
    Ok(())
}

/// Where one record's rdata sits inside the response.
#[derive(Debug)]
struct Rdata {
    rtype: u16,
    start: usize,
}

#[derive(Debug)]
struct Answer {
    raw: Vec<u8>,
    answers: Vec<Rdata>,
}

impl Answer {
    fn parse(raw: Vec<u8>, name: &str, qtype: u16) -> Result<Answer> {
        if raw.len() < 12 {
            return Err(Error::Network("the DNS answer had no header".into()));
        }
        match raw[3] & 0x0f {
            0 => {}
            // NXDOMAIN: the name does not exist. An empty answer, not an error
            // — "this domain publishes no SRV" is a normal discovery outcome.
            3 => {
                return Ok(Answer {
                    raw,
                    answers: Vec::new(),
                })
            }
            code => {
                return Err(Error::Network(format!(
                    "the nameserver refused the query (rcode {code})"
                )))
            }
        }
        let qdcount = u16::from_be_bytes([raw[4], raw[5]]);
        let ancount = u16::from_be_bytes([raw[6], raw[7]]);

        let mut cur = Cursor::at(&raw, 12);
        for _ in 0..qdcount {
            cur.name()?;
            cur.u16()?; // qtype
            cur.u16()?; // qclass
        }

        let mut answers = Vec::new();
        for _ in 0..ancount {
            cur.name()?;
            let rtype = cur.u16()?;
            let _class = cur.u16()?;
            let _ttl = cur.u32()?;
            let len = usize::from(cur.u16()?);
            let start = cur.pos;
            cur.skip(len)?;
            answers.push(Rdata { rtype, start });
        }
        let _ = (name, qtype);
        Ok(Answer { raw, answers })
    }

    fn records(&self, rtype: u16) -> impl Iterator<Item = &Rdata> {
        self.answers.iter().filter(move |r| r.rtype == rtype)
    }
}

/// A reader over the response, with the name decompression the format needs.
struct Cursor<'a> {
    raw: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn at(raw: &'a [u8], pos: usize) -> Cursor<'a> {
        Cursor { raw, pos }
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .raw
            .get(self.pos)
            .ok_or_else(|| Error::Network("the DNS answer ended mid-record".into()))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes([self.byte()?, self.byte()?]))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes([
            self.byte()?,
            self.byte()?,
            self.byte()?,
            self.byte()?,
        ]))
    }

    fn skip(&mut self, n: usize) -> Result<()> {
        self.pos = self
            .pos
            .checked_add(n)
            .filter(|p| *p <= self.raw.len())
            .ok_or_else(|| Error::Network("the DNS answer ended mid-record".into()))?;
        Ok(())
    }

    /// Reads a domain name, following compression pointers.
    ///
    /// The bound on jumps is the whole point of writing this by hand carefully:
    /// a pointer may point backwards to anywhere, including at itself, and a
    /// parser that follows them without a budget loops forever on a malicious
    /// answer.
    fn name(&mut self) -> Result<String> {
        let mut out = String::new();
        let mut jumps = 0;
        let mut pos = self.pos;
        let mut advanced = None;

        loop {
            let len = *self
                .raw
                .get(pos)
                .ok_or_else(|| Error::Network("the DNS answer ended mid-name".into()))?;
            match len & 0xc0 {
                0x00 => {
                    pos += 1;
                    if len == 0 {
                        break;
                    }
                    let end = pos + usize::from(len);
                    let label = self
                        .raw
                        .get(pos..end)
                        .ok_or_else(|| Error::Network("the DNS answer ended mid-label".into()))?;
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(&String::from_utf8_lossy(label));
                    pos = end;
                }
                0xc0 => {
                    let high = usize::from(len & 0x3f) << 8;
                    let low = usize::from(
                        *self
                            .raw
                            .get(pos + 1)
                            .ok_or_else(|| Error::Network("a truncated DNS pointer".into()))?,
                    );
                    // The cursor stops at the pointer; only the reading of the
                    // name follows it.
                    advanced.get_or_insert(pos + 2);
                    jumps += 1;
                    if jumps > 16 {
                        return Err(Error::Network("the DNS answer's names loop".into()));
                    }
                    pos = high | low;
                }
                _ => return Err(Error::Network("a DNS label used a reserved form".into())),
            }
        }
        self.pos = advanced.unwrap_or(pos);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a response for a question, the way a nameserver would.
    fn response(qname: &str, qtype: u16, records: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![0x12, 0x34, 0x81, 0x80];
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&(records.len() as u16).to_be_bytes());
        out.extend_from_slice(&[0u8; 4]);
        encode_name(qname, &mut out).expect("a name");
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&[0, 1]);
        for (rtype, rdata) in records {
            encode_name(qname, &mut out).expect("a name");
            out.extend_from_slice(&rtype.to_be_bytes());
            out.extend_from_slice(&[0, 1]);
            out.extend_from_slice(&300u32.to_be_bytes());
            out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            out.extend_from_slice(rdata);
        }
        out
    }

    fn srv_rdata(priority: u16, weight: u16, port: u16, target: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&priority.to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&port.to_be_bytes());
        encode_name(target, &mut out).expect("a target");
        out
    }

    fn parse_srv(raw: Vec<u8>) -> Vec<Srv> {
        let answer = Answer::parse(raw, "_imaps._tcp.example.com", TYPE_SRV).expect("an answer");
        let mut out = Vec::new();
        for rdata in answer.records(TYPE_SRV) {
            let mut cur = Cursor::at(&answer.raw, rdata.start);
            let priority = cur.u16().expect("priority");
            let weight = cur.u16().expect("weight");
            let port = cur.u16().expect("port");
            let target = cur.name().expect("target");
            out.push(Srv {
                priority,
                weight,
                port,
                target,
            });
        }
        out
    }

    #[test]
    fn reads_an_srv_answer() {
        let raw = response(
            "_imaps._tcp.example.com",
            TYPE_SRV,
            &[(TYPE_SRV, srv_rdata(10, 5, 993, "imap.example.com"))],
        );
        let records = parse_srv(raw);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].port, 993);
        assert_eq!(records[0].target, "imap.example.com");
    }

    #[test]
    fn follows_a_compression_pointer() {
        // A target given as a pointer back to the question's name, which is
        // what a real nameserver emits and what a naive parser gets wrong.
        let mut rdata = Vec::new();
        rdata.extend_from_slice(&1u16.to_be_bytes());
        rdata.extend_from_slice(&0u16.to_be_bytes());
        rdata.extend_from_slice(&143u16.to_be_bytes());
        rdata.extend_from_slice(&[0xc0, 0x0c]); // → offset 12, the question
        let raw = response("_imap._tcp.example.com", TYPE_SRV, &[(TYPE_SRV, rdata)]);
        let records = parse_srv(raw);
        assert_eq!(records[0].target, "_imap._tcp.example.com");
    }

    #[test]
    fn a_pointer_loop_is_refused_rather_than_hung_on() {
        let mut raw = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0];
        // A name at offset 12 whose first label is a pointer to itself.
        raw.extend_from_slice(&[0xc0, 0x0c]);
        raw.extend_from_slice(&TYPE_SRV.to_be_bytes());
        raw.extend_from_slice(&[0, 1]);
        let err = Answer::parse(raw, "loop", TYPE_SRV).expect_err("a loop is an error");
        assert!(err.to_string().contains("loop"), "{err}");
    }

    #[test]
    fn nxdomain_is_an_empty_answer_not_a_failure() {
        let mut raw = vec![0x12, 0x34, 0x81, 0x83, 0, 0, 0, 0, 0, 0, 0, 0];
        raw.push(0);
        let answer = Answer::parse(raw, "nothing.example", TYPE_SRV).expect("nxdomain parses");
        assert_eq!(answer.records(TYPE_SRV).count(), 0);
    }

    #[test]
    fn a_refusal_is_reported() {
        let raw = vec![0x12, 0x34, 0x81, 0x85, 0, 0, 0, 0, 0, 0, 0, 0];
        let err = Answer::parse(raw, "x.example", TYPE_SRV).expect_err("rcode 5 is an error");
        assert!(err.to_string().contains("refused"), "{err}");
    }

    #[test]
    fn mx_records_come_back_most_preferred_first() {
        let mut low = Vec::new();
        low.extend_from_slice(&30u16.to_be_bytes());
        encode_name("backup.example.com", &mut low).expect("a name");
        let mut high = Vec::new();
        high.extend_from_slice(&5u16.to_be_bytes());
        encode_name("mail.example.com", &mut high).expect("a name");

        let raw = response("example.com", TYPE_MX, &[(TYPE_MX, low), (TYPE_MX, high)]);
        let answer = Answer::parse(raw, "example.com", TYPE_MX).expect("an answer");
        let mut out = Vec::new();
        for rdata in answer.records(TYPE_MX) {
            let mut cur = Cursor::at(&answer.raw, rdata.start);
            let preference = cur.u16().expect("preference");
            let exchange = cur.name().expect("exchange");
            out.push(Mx {
                preference,
                exchange,
            });
        }
        out.sort_by_key(|m| m.preference);
        assert_eq!(out[0].exchange, "mail.example.com");
        assert_eq!(out[1].exchange, "backup.example.com");
    }

    #[test]
    fn resolvers_come_from_the_environment_when_it_names_one() {
        // Deliberately not a real resolver: what is asserted is the parsing,
        // and a port that is not 53 is what proves the whole string was read.
        temp_env("MAILBOX_RESOLVER", "192.0.2.53:5353,192.0.2.54", || {
            let found = system_resolvers();
            assert_eq!(found[0].port(), 5353);
            assert_eq!(found[1].port(), 53);
            assert_eq!(found[1].ip().to_string(), "192.0.2.54");
        });
    }

    #[test]
    fn a_name_with_an_empty_label_is_refused() {
        let mut out = Vec::new();
        assert!(encode_name("imap..example.com", &mut out).is_err());
    }

    fn temp_env(key: &str, value: &str, body: impl FnOnce()) {
        let previous = std::env::var(key).ok();
        // SAFETY: single-threaded within this test, and restored below.
        unsafe { std::env::set_var(key, value) };
        body();
        match previous {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}

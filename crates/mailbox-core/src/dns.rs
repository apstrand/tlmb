//! A very small DNS client, for the `SRV` and `MX` lookups autoconfiguration
//! needs (§9).
//!
//! Hand-rolled, like the IMAP, SMTP and HTTPS clients and for the same reason
//! (§4): a general resolver is a large dependency — async runtime, caching,
//! DNSSEC, EDNS negotiation — to carry into a static binary for the two record
//! types `accounts add` asks about. What is here is that subset: one question
//! per query, `IN` class, `SRV` and `MX` only, UDP with a TCP retry when the
//! answer comes back truncated.
//!
//! No DNSSEC validation, so what this returns is a *hint*, not an
//! authorisation. Discovery uses it to fill a form the user still confirms,
//! and the TLS certificate — checked against the hostname the user ends up
//! with — is what actually decides whether the server is the right one.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use crate::error::{Error, Result};

/// How long to wait on one server. Discovery runs while someone watches a
/// spinner, and there may be two nameservers to get through, so this is short.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Answers larger than this are a mistake or an attack. A handful of SRV
/// records is a few hundred bytes.
const MAX_MESSAGE: usize = 8 * 1024;

const TYPE_MX: u16 = 15;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;

/// One `SRV` record: where a service lives, and how much the domain wants you
/// to prefer it.
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

/// Looks up `SRV` records for `name`, best first.
///
/// A single `.` target is RFC 2782's "the service is decidedly not available
/// here"; it is dropped rather than reported, because a caller that wanted to
/// know would have to special-case it anyway.
pub fn srv(name: &str) -> Result<Vec<Srv>> {
    let mut out = Vec::new();
    for record in query(name, TYPE_SRV)? {
        if record.len() < 7 {
            continue;
        }
        let target = read_name(&record.message, record.start + 6)?.0;
        if target == "." || target.is_empty() {
            continue;
        }
        out.push(Srv {
            priority: be16(&record.data()[0..2]),
            weight: be16(&record.data()[2..4]),
            port: be16(&record.data()[4..6]),
            target: strip_root(&target),
        });
    }
    // Lowest priority wins; within a priority, the heavier target. Real
    // weighted random selection is for a client making many connections, not
    // for filling in a settings form once.
    out.sort_by(|a, b| a.priority.cmp(&b.priority).then(b.weight.cmp(&a.weight)));
    Ok(out)
}

/// Looks up `MX` records for `domain`, most-preferred first.
pub fn mx(domain: &str) -> Result<Vec<Mx>> {
    let mut out = Vec::new();
    for record in query(domain, TYPE_MX)? {
        if record.len() < 3 {
            continue;
        }
        let exchange = read_name(&record.message, record.start + 2)?.0;
        if exchange == "." || exchange.is_empty() {
            continue;
        }
        out.push(Mx {
            preference: be16(&record.data()[0..2]),
            exchange: strip_root(&exchange),
        });
    }
    out.sort_by_key(|m| m.preference);
    Ok(out)
}

/// A resource record still pointing into the message it came from, because
/// `SRV` and `MX` both end in a compressed name that only resolves there.
struct Record {
    message: Vec<u8>,
    start: usize,
    end: usize,
}

impl Record {
    fn data(&self) -> &[u8] {
        &self.message[self.start..self.end]
    }
    fn len(&self) -> usize {
        self.end - self.start
    }
}

/// Sends one question to each configured nameserver until one answers.
fn query(name: &str, rtype: u16) -> Result<Vec<Record>> {
    let servers = nameservers()?;
    if servers.is_empty() {
        return Err(Error::Network(
            "no nameserver is configured, so server settings cannot be looked up".into(),
        ));
    }
    let question = encode_query(name, rtype)?;
    let mut last: Option<Error> = None;
    for server in &servers {
        match ask(*server, &question, name, rtype) {
            Ok(records) => return Ok(records),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| Error::Network("the nameserver did not answer".into())))
}

fn ask(server: IpAddr, question: &[u8], name: &str, rtype: u16) -> Result<Vec<Record>> {
    let addr = SocketAddr::new(server, 53);
    let message = match ask_udp(addr, question) {
        // A truncated answer is the one case worth a second round trip: the
        // records are there, they just did not fit in a datagram.
        Ok(m) if truncated(&m) => ask_tcp(addr, question)?,
        Ok(m) => m,
        Err(e) => return Err(e),
    };
    parse_answers(message, name, rtype)
}

fn ask_udp(addr: SocketAddr, question: &[u8]) -> Result<Vec<u8>> {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.set_write_timeout(Some(TIMEOUT))?;
    socket.connect(addr)?;
    socket.send(question)?;
    let mut buf = vec![0u8; MAX_MESSAGE];
    let n = socket.recv(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn ask_tcp(addr: SocketAddr, question: &[u8]) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    // DNS over TCP prefixes every message with its length (RFC 1035 §4.2.2).
    let len = u16::try_from(question.len())
        .map_err(|_| Error::Network("the DNS question is too long".into()))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(question)?;
    stream.flush()?;

    let mut header = [0u8; 2];
    stream.read_exact(&mut header)?;
    let len = usize::from(u16::from_be_bytes(header));
    if len > MAX_MESSAGE {
        return Err(Error::Network(format!(
            "the nameserver sent {len} bytes, more than this client reads"
        )));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(body)
}

fn truncated(message: &[u8]) -> bool {
    message.len() >= 3 && message[2] & 0x02 != 0
}

fn encode_query(name: &str, rtype: u16) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    // A fixed id is fine here and a random one would be security theatre: the
    // spoofing this defends against is defeated by the response arriving on a
    // connected socket from a resolver we chose, and the answer only ever
    // pre-fills a form.
    out.extend_from_slice(&[0x4d, 0x62]);
    // Recursion desired; everything else clear.
    out.extend_from_slice(&[0x01, 0x00]);
    out.extend_from_slice(&1u16.to_be_bytes()); // one question
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // no answers, authority, additional
    encode_name(name, &mut out)?;
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(out)
}

fn encode_name(name: &str, out: &mut Vec<u8>) -> Result<()> {
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() {
            return Err(Error::Usage(format!("`{name}` has an empty label")));
        }
        let len = u8::try_from(label.len())
            .ok()
            .filter(|n| *n <= 63)
            .ok_or_else(|| Error::Usage(format!("`{label}` is too long for a DNS label")))?;
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    Ok(())
}

/// Walks the answer section, keeping the records that answer what we asked.
///
/// `CNAME`s in the answer section are skipped rather than followed: a resolver
/// that returned one also returned the records it points at, in the same
/// message, which is what the type filter picks up.
fn parse_answers(message: Vec<u8>, name: &str, rtype: u16) -> Result<Vec<Record>> {
    if message.len() < 12 {
        return Err(Error::Network("the DNS answer was truncated".into()));
    }
    match message[3] & 0x0f {
        0 => {}
        // NXDOMAIN. Not an error to the caller: a domain with no SRV records
        // is the common case, not a failure.
        3 => return Ok(Vec::new()),
        code => {
            return Err(Error::Network(format!(
                "the nameserver refused the query for `{name}` (rcode {code})"
            )))
        }
    }

    let questions = be16(&message[4..6]);
    let answers = be16(&message[6..8]);
    let mut at = 12;
    for _ in 0..questions {
        at = skip_name(&message, at)?;
        at = at
            .checked_add(4)
            .ok_or_else(|| Error::Network("the DNS answer was truncated".into()))?;
    }

    let mut out = Vec::new();
    for _ in 0..answers {
        at = skip_name(&message, at)?;
        if at + 10 > message.len() {
            return Err(Error::Network("the DNS answer was truncated".into()));
        }
        let this_type = be16(&message[at..at + 2]);
        let this_class = be16(&message[at + 2..at + 4]);
        let rdlength = usize::from(be16(&message[at + 8..at + 10]));
        let start = at + 10;
        let end = start
            .checked_add(rdlength)
            .filter(|e| *e <= message.len())
            .ok_or_else(|| Error::Network("the DNS answer was truncated".into()))?;
        if this_type == rtype && this_class == CLASS_IN {
            out.push(Record {
                message: message.clone(),
                start,
                end,
            });
        }
        at = end;
    }
    Ok(out)
}

/// Reads a name at `at`, following compression pointers. Returns the name and
/// the offset just past it *in the record* — which is not where it ends when a
/// pointer was involved, so callers that need to keep walking use
/// [`skip_name`] instead.
fn read_name(message: &[u8], at: usize) -> Result<(String, usize)> {
    let mut out = String::new();
    let mut at = at;
    // A pointer may only point backwards, so bounding the hops by the message
    // length is enough to stop a loop.
    let mut hops = 0;
    let mut end = None;
    loop {
        if hops > message.len() {
            return Err(Error::Network("the DNS answer has a name loop".into()));
        }
        let len = *message
            .get(at)
            .ok_or_else(|| Error::Network("the DNS answer was truncated".into()))?;
        match len & 0xc0 {
            0 => {
                if len == 0 {
                    if end.is_none() {
                        end = Some(at + 1);
                    }
                    break;
                }
                let start = at + 1;
                let stop = start + usize::from(len);
                let label = message
                    .get(start..stop)
                    .ok_or_else(|| Error::Network("the DNS answer was truncated".into()))?;
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(&String::from_utf8_lossy(label));
                at = stop;
            }
            0xc0 => {
                let next = message
                    .get(at + 1)
                    .ok_or_else(|| Error::Network("the DNS answer was truncated".into()))?;
                let target = usize::from(u16::from(len & 0x3f) << 8 | u16::from(*next));
                if target >= at {
                    return Err(Error::Network(
                        "the DNS answer has a forward pointer".into(),
                    ));
                }
                if end.is_none() {
                    end = Some(at + 2);
                }
                at = target;
                hops += 1;
            }
            _ => {
                return Err(Error::Network(
                    "the DNS answer has an unknown label type".into(),
                ))
            }
        }
    }
    Ok((out, end.unwrap_or(at)))
}

fn skip_name(message: &[u8], at: usize) -> Result<usize> {
    Ok(read_name(message, at)?.1)
}

fn strip_root(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// The system's nameservers, from `/etc/resolv.conf`.
///
/// iOS keeps a real one (a symlink into `/var/run`) that tracks whatever the
/// current interface hands out, so the same three lines of parsing serve the
/// phone and the desktop. Nothing is hard-coded as a fallback on purpose: a
/// client that quietly asked a public resolver when the configured one was
/// unreachable would leak which mail domains someone is setting up.
fn nameservers() -> Result<Vec<IpAddr>> {
    let text = std::fs::read_to_string("/etc/resolv.conf")?;
    Ok(parse_resolv_conf(&text))
}

fn parse_resolv_conf(text: &str) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let line = line.split(';').next().unwrap_or("");
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        let Some(raw) = fields.next() else { continue };
        // A scoped link-local address (`fe80::1%en0`) parses once the zone is
        // off; the zone is not something this client can use anyway.
        let raw = raw.split('%').next().unwrap_or(raw);
        if let Ok(ip) = raw.parse::<IpAddr>() {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a DNS response message the parser can be pointed at.
    fn message(answers: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut m = vec![0x4d, 0x62, 0x81, 0x80];
        m.extend_from_slice(&1u16.to_be_bytes());
        m.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0]);
        encode_name("example.com", &mut m).unwrap();
        m.extend_from_slice(&TYPE_SRV.to_be_bytes());
        m.extend_from_slice(&CLASS_IN.to_be_bytes());
        for (rtype, rdata) in answers {
            // Point the owner name at the question, exercising compression.
            m.extend_from_slice(&[0xc0, 0x0c]);
            m.extend_from_slice(&rtype.to_be_bytes());
            m.extend_from_slice(&CLASS_IN.to_be_bytes());
            m.extend_from_slice(&300u32.to_be_bytes());
            m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            m.extend_from_slice(rdata);
        }
        m
    }

    fn srv_rdata(priority: u16, weight: u16, port: u16, target: &str) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&priority.to_be_bytes());
        d.extend_from_slice(&weight.to_be_bytes());
        d.extend_from_slice(&port.to_be_bytes());
        encode_name(target, &mut d).unwrap();
        d
    }

    fn parse_srv(m: Vec<u8>) -> Vec<Srv> {
        let records = parse_answers(m, "example.com", TYPE_SRV).unwrap();
        let mut out = Vec::new();
        for record in records {
            let target = read_name(&record.message, record.start + 6).unwrap().0;
            out.push(Srv {
                priority: be16(&record.data()[0..2]),
                weight: be16(&record.data()[2..4]),
                port: be16(&record.data()[4..6]),
                target: strip_root(&target),
            });
        }
        out
    }

    #[test]
    fn reads_an_srv_record() {
        let m = message(&[(TYPE_SRV, srv_rdata(10, 5, 993, "imap.example.com"))]);
        assert_eq!(
            parse_srv(m),
            vec![Srv {
                priority: 10,
                weight: 5,
                port: 993,
                target: "imap.example.com".into(),
            }]
        );
    }

    #[test]
    fn skips_records_of_other_types() {
        let m = message(&[
            (TYPE_MX, {
                let mut d = 10u16.to_be_bytes().to_vec();
                encode_name("mx.example.com", &mut d).unwrap();
                d
            }),
            (TYPE_SRV, srv_rdata(1, 0, 993, "imap.example.com")),
        ]);
        assert_eq!(parse_srv(m).len(), 1);
    }

    #[test]
    fn an_nxdomain_is_an_empty_answer_not_an_error() {
        let mut m = message(&[]);
        m[3] = 0x83;
        assert!(parse_answers(m, "example.com", TYPE_SRV)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_refusal_is_an_error() {
        let mut m = message(&[]);
        m[3] = 0x85;
        assert!(parse_answers(m, "example.com", TYPE_SRV).is_err());
    }

    #[test]
    fn a_forward_pointer_is_refused() {
        // Left alone, a name pointing at or past itself loops the reader.
        let buf = [0xc0u8, 0x00];
        assert!(read_name(&buf, 0).is_err());
    }

    #[test]
    fn a_backward_pointer_is_followed() {
        let mut buf = Vec::new();
        encode_name("imap.example.com", &mut buf).unwrap();
        let at = buf.len();
        buf.extend_from_slice(&[0xc0, 0x00]);
        let (name, end) = read_name(&buf, at).unwrap();
        assert_eq!(name, "imap.example.com");
        // The caller keeps walking after the pointer, not after the target.
        assert_eq!(end, at + 2);
    }

    #[test]
    fn truncation_is_visible_in_the_header() {
        let mut m = message(&[]);
        assert!(!truncated(&m));
        m[2] |= 0x02;
        assert!(truncated(&m));
    }

    #[test]
    fn reads_nameservers_and_ignores_the_rest() {
        let conf = "\
# a comment
domain example.com
nameserver 192.0.2.1
nameserver fe80::1%en0
nameserver 192.0.2.1
options edns0
nameserver not-an-address
";
        assert_eq!(
            parse_resolv_conf(conf),
            vec![
                "192.0.2.1".parse::<IpAddr>().unwrap(),
                "fe80::1".parse::<IpAddr>().unwrap()
            ]
        );
    }
}

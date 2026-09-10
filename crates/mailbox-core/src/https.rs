//! A very small HTTPS client, for the OAuth2 token endpoint (§9).
//!
//! Hand-rolled, like the IMAP and SMTP clients and for the same reason (§4): a
//! full HTTP stack is a large dependency to carry into a static binary for the
//! two `POST`s an OAuth flow makes. What is here is the subset those need —
//! one request, one response, `Content-Length` or `chunked`, no redirects, no
//! keep-alive, no compression.
//!
//! `post_form` follows no redirects on purpose. A token endpoint does not
//! redirect, and a client that followed one would happily carry a client id and
//! an authorization code to wherever it was pointed. `get` does follow them,
//! bounded and never down to plaintext: autoconfig discovery (§9) reads a
//! public document from a host that has every reason to redirect — `www.`,
//! a moved path, a hosting provider — and it carries no secret to lose.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{Error, Result};

/// How long to wait on the whole exchange. An interactive login is behind
/// this, so it is short enough that a black hole is reported rather than hung
/// on.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The largest response body accepted. Token responses are a few hundred
/// bytes; anything at this size is a mistake or an attack.
const MAX_BODY: usize = 1 << 20;

/// How many redirects `get` will follow before calling it a loop.
const MAX_REDIRECTS: usize = 5;

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// The `Location` header, when there was one.
    pub location: Option<String>,
}

impl Response {
    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|e| {
            Error::Network(format!(
                "the response was not JSON ({e}): {}",
                String::from_utf8_lossy(&self.body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ))
        })
    }
}

/// `POST` a form-encoded body and read the response.
///
/// Plaintext is refused unless the host is loopback, which is §9's rule for
/// every other connection this program makes. That exception is what lets the
/// L3 authorization server exist without a certificate.
pub fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Response> {
    let body = encode_form(fields);
    send(
        url,
        "POST",
        Some(("application/x-www-form-urlencoded", body.as_bytes())),
        "application/json",
    )
}

/// `POST` an XML body — what Microsoft's Autodiscover asks for (§9).
pub fn post_xml(url: &str, body: &str) -> Result<Response> {
    send(url, "POST", Some(("text/xml", body.as_bytes())), "text/xml")
}

/// `POST` opaque bytes — what the pairing relay carries (§8.2).
///
/// The relay never learns what these are, so there is no content type worth
/// claiming beyond "bytes".
pub fn post_bytes(url: &str, body: &[u8]) -> Result<Response> {
    send(
        url,
        "POST",
        Some(("application/octet-stream", body)),
        "application/octet-stream",
    )
}

/// `GET` a document, following up to [`MAX_REDIRECTS`] redirects.
pub fn get(url: &str) -> Result<Response> {
    let mut url = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let response = send(&url, "GET", None, "application/xml, text/xml, */*")?;
        match response.location.clone() {
            Some(next) if (300..400).contains(&response.status) => {
                url = resolve(&url, &next)?;
            }
            _ => return Ok(response),
        }
    }
    Err(Error::Network(format!("{url} redirects in a loop")))
}

fn send(url: &str, method: &str, body: Option<(&str, &[u8])>, accept: &str) -> Result<Response> {
    let target = Target::parse(url)?;
    let mut request = format!(
        "{} {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: mailbox/{}\r\n\
         Accept: {}\r\n\
         Connection: close\r\n",
        method,
        target.path,
        target.host_header(),
        env!("CARGO_PKG_VERSION"),
        accept,
    );
    let body = match body {
        Some((content_type, body)) => {
            request.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
                body.len()
            ));
            body
        }
        None => b"",
    };
    request.push_str("\r\n");

    let tcp = connect(&target)?;
    if target.tls {
        let mut stream = crate::tls::stream(&target.host, tcp)?;
        exchange(&mut stream, request.as_bytes(), body)
    } else {
        let mut stream = tcp;
        exchange(&mut stream, request.as_bytes(), body)
    }
}

/// Turns a `Location` into an absolute URL against the request that produced it.
///
/// Absolute, host-relative, and path-relative forms all appear in the wild.
/// A redirect that leaves HTTPS is refused by `Target::parse` on the next hop,
/// which is where §9's rule lives rather than being restated here.
fn resolve(base: &str, location: &str) -> Result<String> {
    if location.contains("://") {
        return Ok(location.to_string());
    }
    let target = Target::parse(base)?;
    let scheme = if target.tls { "https" } else { "http" };
    let authority = target.host_header();
    if let Some(rest) = location.strip_prefix('/') {
        return Ok(format!("{scheme}://{authority}/{rest}"));
    }
    let dir = match target.path.rfind('/') {
        Some(i) => &target.path[..=i],
        None => "/",
    };
    Ok(format!("{scheme}://{authority}{dir}{location}"))
}

fn connect(target: &Target) -> Result<TcpStream> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .map_err(|e| Error::Network(format!("connecting to {}: {e}", target.host)))?;
    tcp.set_read_timeout(Some(TIMEOUT)).ok();
    tcp.set_write_timeout(Some(TIMEOUT)).ok();
    Ok(tcp)
}

fn exchange<S: Read + Write>(stream: &mut S, head: &[u8], body: &[u8]) -> Result<Response> {
    stream
        .write_all(head)
        .and_then(|_| stream.write_all(body))
        .and_then(|_| stream.flush())
        .map_err(|e| Error::Network(format!("writing the request: {e}")))?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_BODY {
                    return Err(Error::Network("the response was implausibly large".into()));
                }
                if let Some(response) = complete(&raw, false)? {
                    return Ok(response);
                }
            }
            Err(e) => return Err(Error::Network(format!("reading the response: {e}"))),
        }
    }
    complete(&raw, true)?.ok_or_else(|| Error::Network("the connection closed mid-response".into()))
}

/// Parses `raw` if a whole response is there yet, else `None`.
///
/// `eof` says the connection has closed, which is itself a framing: a response
/// with neither `Content-Length` nor chunking ends when the socket does.
fn complete(raw: &[u8], eof: bool) -> Result<Option<Response>> {
    let Some(split) = find(raw, b"\r\n\r\n") else {
        return Ok(None);
    };
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| Error::Network("the response had no status line".into()))?;

    let mut length: Option<usize> = None;
    let mut chunked = false;
    let mut location = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok(),
            "transfer-encoding" if value.trim().eq_ignore_ascii_case("chunked") => chunked = true,
            "location" => location = Some(value.trim().to_string()),
            _ => {}
        }
    }

    let rest = &raw[split + 4..];
    if chunked {
        return Ok(dechunk(rest).map(|body| Response {
            status,
            body,
            location,
        }));
    }
    match length {
        Some(n) if rest.len() >= n => Ok(Some(Response {
            status,
            body: rest[..n].to_vec(),
            location,
        })),
        Some(_) => Ok(None),
        // No length and no chunking: the body ends when the connection does.
        None if eof => Ok(Some(Response {
            status,
            body: rest.to_vec(),
            location,
        })),
        None => Ok(None),
    }
}

fn dechunk(mut rest: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let end = find(rest, b"\r\n")?;
        let size = usize::from_str_radix(
            String::from_utf8_lossy(&rest[..end])
                .split(';')
                .next()?
                .trim(),
            16,
        )
        .ok()?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Some(body);
        }
        if rest.len() < size + 2 {
            return None;
        }
        body.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

struct Target {
    host: String,
    port: u16,
    path: String,
    tls: bool,
}

impl Target {
    fn parse(url: &str) -> Result<Target> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| Error::usage(format!("`{url}` is not an absolute URL")))?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            other => {
                return Err(Error::usage(format!(
                    "`{other}` is not a URL scheme this speaks"
                )))
            }
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse()
                    .map_err(|_| Error::usage(format!("`{p}` is not a port")))?,
            ),
            None => (authority.to_string(), if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(Error::usage(format!("`{url}` has no host")));
        }
        // §9: plaintext only to loopback, and only because the alternative is
        // a test suite that cannot exist.
        if !tls && !crate::accounts::is_loopback(&host) {
            return Err(Error::usage(format!(
                "`{url}` is plaintext to a non-loopback host, which §9 refuses"
            )));
        }
        Ok(Target {
            host,
            port,
            path: path.to_string(),
            tls,
        })
    }

    fn host_header(&self) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn encode_form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `application/x-www-form-urlencoded`, which is not quite percent-encoding:
/// a space is `+`, and everything outside the unreserved set is escaped.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

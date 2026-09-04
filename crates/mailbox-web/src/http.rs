//! Just enough HTTP/1.1 to serve one user's mail to one browser.
//!
//! Hand-written, like the IMAP client of §5.3 and for a related reason. A
//! server framework brings an async runtime, a TLS stack, and a few hundred
//! transitive crates to a binary §4 wants self-contained and §10 caps at 25 MB
//! — to answer requests from `localhost` for a single person. What is needed
//! here is a request line, some headers, a body, and the discipline to bound
//! all three.
//!
//! Bounded is the operative word. Everything below has a limit, and the limits
//! are the security posture: this listens on a socket, and while it defaults to
//! loopback, §3 lets the user bind it to the LAN.

use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The longest request line we will read: a path plus a query string.
const MAX_REQUEST_LINE: usize = 8 * 1024;
/// Enough for a browser's usual dozen, and no more.
const MAX_HEADERS: usize = 64;
const MAX_HEADER_LINE: usize = 8 * 1024;
/// Bodies are JSON commands — ids and flags, not attachments.
const MAX_BODY: usize = 1024 * 1024;
/// A connection that has said nothing for this long is not a browser.
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The path, percent-decoded, with the query string removed.
    pub path: String,
    /// Parsed query parameters, percent-decoded.
    pub query: BTreeMap<String, String>,
    /// Header names lowercased, since HTTP's are case-insensitive and code
    /// that forgets that works until the day a client capitalises differently.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// One cookie's value, from the `Cookie` header.
    pub fn cookie(&self, name: &str) -> Option<String> {
        let header = self.header("cookie")?;
        header.split(';').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
    }

    /// Reads one request. `Ok(None)` means the client closed politely.
    pub fn read(stream: &mut BufReader<TcpStream>) -> Result<Option<Request>, Error> {
        let mut line = String::new();
        let n = read_line(stream, &mut line, MAX_REQUEST_LINE)?;
        if n == 0 {
            return Ok(None);
        }

        let mut parts = line.trim_end().split(' ');
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let version = parts.next().unwrap_or_default();
        if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
            return Err(Error::Malformed("not an HTTP request line"));
        }

        let (raw_path, raw_query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target.as_str(), ""),
        };

        let mut headers = BTreeMap::new();
        for _ in 0..MAX_HEADERS {
            let mut header = String::new();
            if read_line(stream, &mut header, MAX_HEADER_LINE)? == 0 {
                return Err(Error::Malformed("headers ended without a blank line"));
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else {
                return Err(Error::Malformed("a header with no colon"));
            };
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }

        let length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if length > MAX_BODY {
            return Err(Error::TooLarge);
        }
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body)?;

        Ok(Some(Request {
            method,
            path: percent_decode(raw_path),
            query: parse_query(raw_query),
            headers,
            body,
        }))
    }
}

/// What to send back.
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    /// Extra headers, in order. `Set-Cookie` may appear more than once, so
    /// this is a list rather than a map.
    pub extra: Vec<(String, String)>,
    /// Whether this response may be put in a frame.
    ///
    /// False everywhere but the one route whose whole purpose is to be framed
    /// — the message body — and that one names its permitted ancestors in its
    /// own policy instead.
    pub frameable: bool,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response {
            status,
            content_type: content_type.to_string(),
            body,
            extra: Vec::new(),
            frameable: false,
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Response {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Response::new(status, "application/json; charset=utf-8", body)
    }

    pub fn text(status: u16, body: &str) -> Response {
        Response::new(
            status,
            "text/plain; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }

    pub fn with(mut self, name: &str, value: &str) -> Response {
        self.extra.push((name.to_string(), value.to_string()));
        self
    }

    /// Allows this response to be framed. See [`Response::frameable`].
    pub fn framed(mut self) -> Response {
        self.frameable = true;
        self
    }

    pub fn write(&self, out: &mut impl Write) -> std::io::Result<()> {
        write!(
            out,
            "HTTP/1.1 {} {}\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n",
            self.status,
            reason(self.status),
            self.content_type,
            self.body.len(),
        )?;
        // §9's posture, applied to every response rather than remembered per
        // route: nothing here is embeddable, nothing sniffs, nothing leaks a
        // URL to a third party — because there is no third party.
        write!(
            out,
            "X-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\n\
             Cache-Control: no-store\r\n"
        )?;
        if !self.frameable {
            write!(out, "X-Frame-Options: DENY\r\n")?;
        }
        for (name, value) in &self.extra {
            write!(out, "{name}: {value}\r\n")?;
        }
        write!(out, "\r\n")?;
        out.write_all(&self.body)?;
        out.flush()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Malformed(&'static str),
    TooLarge,
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Malformed(what) => write!(f, "malformed request: {what}"),
            Error::TooLarge => write!(f, "request body too large"),
        }
    }
}

/// Reads one CRLF-terminated line, refusing one that never ends.
///
/// `BufRead::read_line` would happily grow a `String` until the process dies,
/// which is a denial of service written as a convenience.
fn read_line(
    stream: &mut BufReader<TcpStream>,
    out: &mut String,
    limit: usize,
) -> Result<usize, Error> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    buf.push(b'\n');
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > limit {
                    return Err(Error::TooLarge);
                }
            }
            Err(e) => return Err(Error::Io(e)),
        }
    }
    if buf.is_empty() {
        return Ok(0);
    }
    *out = String::from_utf8_lossy(&buf).into_owned();
    Ok(buf.len())
}

/// Reads and discards what the client is still sending, so the close is a FIN.
///
/// Bounded twice over — by a byte count and by a short timeout — because the
/// point is politeness to a client mid-write, not willingness to read whatever
/// a stranger wants to send after being refused.
pub fn drain(stream: &mut BufReader<TcpStream>) {
    const MAX_DRAIN: usize = 64 * 1024;
    let _ = stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(200)));

    let mut discard = [0u8; 4096];
    let mut total = 0usize;
    while total < MAX_DRAIN {
        match stream.read(&mut discard) {
            Ok(0) | Err(_) => break,
            Ok(n) => total += n,
        }
    }
}

pub fn set_timeouts(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))
}

fn parse_query(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    out
}

/// Percent-decoding, with `+` meaning a space as a query string does.
///
/// Invalid UTF-8 is replaced rather than rejected: a mangled query parameter
/// should produce "no such folder", not a 500.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

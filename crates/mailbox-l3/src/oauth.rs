//! An OAuth2 authorization server that misbehaves on request (§14.2).
//!
//! The interesting paths in a token flow are the ones a real provider will not
//! perform for you: a PKCE verifier that does not match, a refresh token that
//! has been revoked, a redirect that comes back with somebody else's `state`.
//! Those are exactly the checks whose absence nobody notices, because a flow
//! with no checks at all works perfectly against a cooperative server.
//!
//! Plain HTTP on loopback. §9 permits that for a loopback host and nothing
//! else, which is what lets this exist without a certificate.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What the server should do wrong.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum OauthFault {
    #[default]
    None,
    /// Answer the token request with `invalid_grant`, as a provider does when
    /// a refresh token has been revoked or a code reused.
    InvalidGrant,
    /// Issue an access token but no refresh token — Google's behaviour when
    /// consent was already granted and `prompt=consent` was not sent.
    NoRefreshToken,
    /// Answer `200` with a body that is not JSON at all: a captive portal, or
    /// an error page from something in the middle.
    NotJson,
    /// Answer the token request with an access token that has already expired.
    ExpiredToken,
    /// Close the connection without answering.
    DropTokenRequest,
}

/// One token request the server received, for a test to assert on.
#[derive(Debug, Clone)]
pub struct TokenRequest {
    pub fields: HashMap<String, String>,
}

impl TokenRequest {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

struct State {
    fault: OauthFault,
    /// verifier challenge → the code issued for it, so the server can check
    /// PKCE the way a real one does.
    challenges: HashMap<String, String>,
    requests: Vec<TokenRequest>,
    issued: u32,
}

pub struct OauthServer {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl OauthServer {
    pub fn start() -> std::io::Result<OauthServer> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State {
            fault: OauthFault::None,
            challenges: HashMap::new(),
            requests: Vec::new(),
            issued: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let worker_state = state.clone();
        let worker_stop = stop.clone();
        listener.set_nonblocking(true)?;
        std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = serve(stream, &worker_state);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(OauthServer { addr, state, stop })
    }

    pub fn token_url(&self) -> String {
        format!("http://127.0.0.1:{}/token", self.addr.port())
    }

    /// The authorization endpoint. Nothing serves it — the tests drive the
    /// redirect directly — but a flow needs a URL to build.
    pub fn auth_url(&self) -> String {
        format!("http://127.0.0.1:{}/authorize", self.addr.port())
    }

    pub fn set_fault(&self, fault: OauthFault) {
        self.state.lock().unwrap().fault = fault;
    }

    /// Registers a code as having been issued for a PKCE challenge, standing in
    /// for the browser half of the flow.
    pub fn issue_code(&self, code: &str, challenge: &str) {
        self.state
            .lock()
            .unwrap()
            .challenges
            .insert(code.to_string(), challenge.to_string());
    }

    pub fn requests(&self) -> Vec<TokenRequest> {
        self.state.lock().unwrap().requests.clone()
    }
}

impl Drop for OauthServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut stream: TcpStream, state: &Arc<Mutex<State>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let fields = parse_form(&String::from_utf8_lossy(&body));

    let fault = {
        let mut s = state.lock().unwrap();
        s.requests.push(TokenRequest {
            fields: fields.clone(),
        });
        s.fault.clone()
    };

    if fault == OauthFault::DropTokenRequest {
        return Ok(());
    }

    let (status, body) = respond(&fields, &fault, state);
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn respond(
    fields: &HashMap<String, String>,
    fault: &OauthFault,
    state: &Arc<Mutex<State>>,
) -> (&'static str, String) {
    if *fault == OauthFault::InvalidGrant {
        return (
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#
                .to_string(),
        );
    }
    if *fault == OauthFault::NotJson {
        return (
            "200 OK",
            "<html>a captive portal, probably</html>".to_string(),
        );
    }

    let grant = fields.get("grant_type").map(String::as_str).unwrap_or("");
    if grant == "authorization_code" {
        // What a real provider checks and a lax client never notices:
        // S256(verifier) has to equal the challenge the code was issued for.
        let code = fields.get("code").map(String::as_str).unwrap_or("");
        let verifier = fields
            .get("code_verifier")
            .map(String::as_str)
            .unwrap_or("");
        let expected = state.lock().unwrap().challenges.get(code).cloned();
        match expected {
            Some(challenge) if challenge == s256(verifier) => {}
            Some(_) => return (
                "400 Bad Request",
                r#"{"error":"invalid_grant","error_description":"code_verifier does not match"}"#
                    .to_string(),
            ),
            None => {
                return (
                    "400 Bad Request",
                    r#"{"error":"invalid_grant","error_description":"unknown code"}"#.to_string(),
                )
            }
        }
    }

    let n = {
        let mut s = state.lock().unwrap();
        s.issued += 1;
        s.issued
    };
    let expires_in = if *fault == OauthFault::ExpiredToken {
        -1
    } else {
        3600
    };
    let refresh = if *fault == OauthFault::NoRefreshToken {
        String::new()
    } else {
        format!(r#","refresh_token":"refresh-{n}""#)
    };
    (
        "200 OK",
        format!(
            r#"{{"access_token":"access-{n}","token_type":"Bearer","expires_in":{expires_in}{refresh}}}"#
        ),
    )
}

fn s256(verifier: &str) -> String {
    use base64::Engine;
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize())
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
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

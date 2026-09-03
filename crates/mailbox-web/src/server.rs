//! The listener, the token, and the routing.
//!
//! # Who is allowed in
//!
//! Everything here follows from one fact: this process can read all of
//! someone's mail and send more. §3 permits binding it to the LAN, so the
//! access rules are not a formality.
//!
//! - **Loopback by default**, on an ephemeral port.
//! - **A token, always**, unless the user disables it — which is refused for a
//!   non-loopback bind, since that is precisely the case where a token is the
//!   only thing between a stranger on the café Wi-Fi and the mail.
//! - **The token authenticates the page; a header authenticates the API.** The
//!   cookie is `HttpOnly` and `SameSite=Strict`, so it gates the HTML only, and
//!   every `/api` request must carry `X-Mailbox-Token` instead. A page on
//!   another origin cannot set that header without a CORS preflight, which is
//!   refused — which is what closes the cross-site request forgery that a
//!   cookie-authenticated local API otherwise has wide open.
//! - **Constant-time comparison**, because a token compared with `==` leaks its
//!   prefix to anyone willing to make a few thousand requests.

use std::io::BufReader;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mailbox_core::error::{Error, Result};
use mailbox_core::{Clock, Rng, Tracer, Workspace};

use crate::api;
use crate::assets;
use crate::http::{Request, Response};

/// The name of both the cookie and the header.
pub const TOKEN_NAME: &str = "mailbox_token";
pub const TOKEN_HEADER: &str = "x-mailbox-token";

pub struct Options {
    pub bind: IpAddr,
    pub port: u16,
    /// `None` means the user asked for no token, which is refused off loopback.
    pub token: Option<String>,
    /// Stop after this many requests. Test harness only; `None` in normal use.
    pub max_requests: Option<usize>,
}

pub struct Server {
    listener: TcpListener,
    max_requests: Option<usize>,
    inner: Arc<Inner>,
}

/// Everything a connection needs, shared by the threads serving them.
struct Inner {
    token: Option<String>,
    workspace: Mutex<Workspace>,
    session: mailbox_core::session::SessionOptions,
    tracer: Tracer,
    clock: Clock,
    /// Connections being served right now. A browser opens several at once,
    /// and a cap is what keeps "several" from becoming "as many as anyone
    /// cares to open".
    live: AtomicUsize,
}

/// Enough for a browser's parallel fetches several times over, and few enough
/// that a stranger cannot spend the machine's memory by connecting.
const MAX_CONNECTIONS: usize = 32;

/// A token: 32 bytes of the injected RNG, hex.
///
/// From [`Rng`] rather than straight from the OS so that `MAILBOX_SEED` pins it
/// (§6.7) and a test can predict the URL. That is safe precisely because
/// `MAILBOX_SEED` is unset in normal use, where it comes from the OS.
pub fn make_token(rng: &Rng) -> String {
    mailbox_core::hex::encode(&rng.bytes::<32>())
}

impl Server {
    pub fn bind(
        opts: Options,
        session: mailbox_core::session::SessionOptions,
        tracer: Tracer,
        clock: Clock,
    ) -> Result<Server> {
        if opts.token.is_none() && !opts.bind.is_loopback() {
            return Err(Error::usage(format!(
                "refusing to serve on {} without a token: anything that can reach this address \
                 could read your mail. Drop `--no-token`, or bind to 127.0.0.1 (§3).",
                opts.bind
            )));
        }

        let listener = TcpListener::bind(SocketAddr::new(opts.bind, opts.port)).map_err(|e| {
            Error::usage(format!(
                "could not listen on {}:{}: {e}",
                opts.bind, opts.port
            ))
        })?;

        let workspace = Workspace::open(&session, tracer.clone(), clock.clone())?;

        Ok(Server {
            listener,
            max_requests: opts.max_requests,
            inner: Arc::new(Inner {
                token: opts.token,
                workspace: Mutex::new(workspace),
                session,
                tracer,
                clock,
                live: AtomicUsize::new(0),
            }),
        })
    }

    /// The address actually bound, which is what to print when the port was 0.
    pub fn address(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The URL to open, token and all.
    pub fn url(&self) -> String {
        let addr = match self.address() {
            Ok(a) => a,
            Err(_) => return String::new(),
        };
        // A wildcard bind has no single address to name, so name the machine's
        // loopback and let the user substitute their LAN address — which is
        // what `--bind 0.0.0.0` means they already know.
        let host = if addr.ip().is_unspecified() {
            format!("127.0.0.1:{}", addr.port())
        } else {
            addr.to_string()
        };
        match &self.inner.token {
            Some(t) => format!("http://{host}/?{TOKEN_NAME}={t}"),
            None => format!("http://{host}/"),
        }
    }

    /// Serves until interrupted, or until `max_requests` have been accepted.
    ///
    /// A thread per connection. Serving them one after another looks adequate
    /// for a single user and is not: a browser opens several at once, and the
    /// first thing it asks for is a two-megabyte wasm module. Behind that, the
    /// interface's own first two requests wait — so the mailbox loads empty and
    /// fills in only when something prompts a reload. Concurrency here is not
    /// about throughput; it is about not making the page wait on its own
    /// assets.
    pub fn run(&self) -> Result<()> {
        let mut accepted = 0usize;
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };

            if self.inner.live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                // Refused rather than queued: a queue with no limit is the
                // same problem with a longer fuse.
                let _ = crate::http::Response::text(503, "too many connections")
                    .write(&mut stream.try_clone().unwrap_or(stream));
                continue;
            }

            let inner = self.inner.clone();
            inner.live.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                if let Err(e) = inner.serve_one(stream) {
                    // One bad connection is not a reason to stop serving the
                    // others; a browser that hangs up mid-request is routine.
                    inner.tracer.emit(
                        "web.connection_failed",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
                inner.live.fetch_sub(1, Ordering::SeqCst);
            });

            accepted += 1;
            if self.max_requests.is_some_and(|max| accepted >= max) {
                break;
            }
        }
        Ok(())
    }
}

impl Inner {
    fn serve_one(&self, stream: TcpStream) -> std::result::Result<(), crate::http::Error> {
        crate::http::set_timeouts(&stream)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;

        let response = match Request::read(&mut reader) {
            Ok(Some(request)) => self.route(&request),
            Ok(None) => return Ok(()),
            Err(crate::http::Error::TooLarge) => Response::text(413, "request too large"),
            Err(e) => Response::text(400, &e.to_string()),
        };
        response.write(&mut writer)?;

        // Closing a socket that still holds unread bytes makes the kernel send
        // RST rather than FIN, and the client loses the response it was about
        // to read. That is exactly the case that matters here: the refusals —
        // 413 for an over-long request, 400 for a malformed one — stop reading
        // early *by design*, so without this the client sees "connection reset"
        // instead of being told what was wrong.
        crate::http::drain(&mut reader);
        Ok(())
    }

    fn route(&self, request: &Request) -> Response {
        self.tracer.emit(
            "web.request",
            serde_json::json!({ "method": request.method, "path": request.path }),
        );

        // A preflight is a cross-origin page asking permission. There is no
        // origin that should be talking to this but the page it serves.
        if request.method == "OPTIONS" {
            return Response::text(403, "cross-origin requests are not accepted");
        }

        if let Some(path) = request.path.strip_prefix("/api/") {
            if !self.api_authorized(request) {
                return Response::json(
                    401,
                    &serde_json::json!({
                        "error": {
                            "code": "unauthorized",
                            "message": "this request needs the token from `mailbox serve`",
                        }
                    }),
                );
            }
            return api::handle(
                path,
                request,
                &self.workspace,
                &self.session,
                &self.tracer,
                &self.clock,
            );
        }

        match self.page_token(request) {
            PageAuth::Ok { set_cookie } => {
                let response = assets::serve(&request.path, self.token.as_deref());
                match set_cookie {
                    // `SameSite=Strict` so another site cannot cause a request
                    // that carries it; `HttpOnly` so a bug in the page cannot
                    // read it back out.
                    Some(token) => response.with(
                        "Set-Cookie",
                        &format!(
                            "{TOKEN_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400"
                        ),
                    ),
                    None => response,
                }
            }
            PageAuth::Denied => Response::new(
                401,
                "text/html; charset=utf-8",
                assets::UNAUTHORIZED.as_bytes().to_vec(),
            ),
        }
    }

    /// Whether this API request carries the token, in the header.
    ///
    /// The cookie is deliberately not accepted here. If it were, any page in
    /// the browser could POST to this port and the browser would attach it.
    fn api_authorized(&self, request: &Request) -> bool {
        let Some(expected) = &self.token else {
            return true;
        };
        request
            .header(TOKEN_HEADER)
            .is_some_and(|got| constant_time_eq(got.as_bytes(), expected.as_bytes()))
    }

    fn page_token(&self, request: &Request) -> PageAuth {
        let Some(expected) = &self.token else {
            return PageAuth::Ok { set_cookie: None };
        };
        // In the URL: the first visit, from the link `serve` printed. The
        // cookie that comes back is what makes a reload work without it.
        if let Some(given) = request.query.get(TOKEN_NAME) {
            if constant_time_eq(given.as_bytes(), expected.as_bytes()) {
                return PageAuth::Ok {
                    set_cookie: Some(expected.clone()),
                };
            }
        }
        match request.cookie(TOKEN_NAME) {
            Some(c) if constant_time_eq(c.as_bytes(), expected.as_bytes()) => {
                PageAuth::Ok { set_cookie: None }
            }
            _ => PageAuth::Denied,
        }
    }
}

enum PageAuth {
    Ok { set_cookie: Option<String> },
    Denied,
}

/// Compares without revealing where the difference is.
///
/// `==` on a token returns as soon as two bytes differ, and the time it took
/// says how many matched. That is a practical attack against a local service
/// an attacker can query in a loop, and the fix costs nothing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

//! OAuth2 for IMAP and SMTP (§9): the authorization code flow with PKCE,
//! through the system browser, redirecting to loopback.
//!
//! Using an access token was already built — `XOAUTH2` on both protocols. What
//! is here is getting one and keeping it: the browser round trip, the token
//! exchange, and the refresh that happens when a command finds the access
//! token has expired.
//!
//! **No client id is shipped.** A native application cannot keep a secret, so
//! OAuth for installed apps makes the client id public and relies on PKCE for
//! the rest — but a client id is still an identity registered with the
//! provider by whoever publishes the application, and borrowing another
//! program's is both against every provider's terms and one rotation away from
//! breaking. So `oauth_client_id` is config, the endpoints and scopes below
//! are built in, and the README says how to register one. See §16.
//!
//! ## What is stored, and where
//!
//! The refresh token is a credential and goes where credentials go (§9): the
//! keychain, or `credential_command`, or the environment. The *access* token
//! is short-lived and re-derivable, and caching it matters — without a cache
//! every `mailbox ls` would open a TLS connection to the provider before it
//! could open one to the mail server. It is written to a mode-0600 file under
//! the account's state directory, which is itself 0700, and never to the
//! config or to `meta.db`.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine;

use crate::error::{Error, Result};
use crate::storage::{ByteStorage, FsStorage};

/// A provider's endpoints and the scopes mail needs from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    /// Space-separated, as the parameter is.
    pub scope: &'static str,
    /// Where to register an application, for the error message that matters
    /// most: the one someone hits before they have a client id.
    pub register_url: &'static str,
    /// The credentials this build ships, if it ships any.
    ///
    /// From the environment at *compile* time, so a release can carry the
    /// project's own registration, a distribution can substitute its own, and
    /// a plain `cargo build` carries none and says so. See §16.
    pub builtin: Option<BuiltinClient>,
}

/// A client id, and the parameter Google and Yahoo insist on calling a secret.
///
/// Not a credential, and deliberately not routed through
/// [`crate::credentials`]: for an installed application both halves ship in
/// every copy of the binary, and treating the second one as confidential —
/// keychain, redaction — would imply a protection that does not exist. RFC
/// 8252 is explicit that a native client cannot hold one; PKCE is what
/// actually protects the flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub id: String,
    pub secret: Option<String>,
    /// True when this came from the build rather than from the account's
    /// config, which is the difference between "your quota" and "ours".
    pub builtin: bool,
}

/// The same, in the form a `const` table can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinClient {
    pub id: &'static str,
    pub secret: Option<&'static str>,
}

/// What this build ships for a provider, read from the environment at compile
/// time. `build.rs` asks cargo to rebuild when either variable changes.
macro_rules! builtin {
    ($id_var:literal, $secret_var:literal) => {
        match option_env!($id_var) {
            Some(id) => Some(BuiltinClient {
                id,
                secret: option_env!($secret_var),
            }),
            None => None,
        }
    };
}

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "google",
        name: "Google",
        auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        scope: "https://mail.google.com/",
        register_url: "https://console.cloud.google.com/apis/credentials",
        builtin: builtin!("MAILBOX_GOOGLE_CLIENT_ID", "MAILBOX_GOOGLE_CLIENT_SECRET"),
    },
    Provider {
        id: "microsoft",
        name: "Microsoft",
        auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        // `offline_access` is what makes a refresh token come back; without it
        // the login has to be repeated every hour.
        scope: "offline_access https://outlook.office.com/IMAP.AccessAsUser.All \
                https://outlook.office.com/SMTP.Send",
        register_url: "https://entra.microsoft.com/#view/Microsoft_AAD_RegisteredApps",
        builtin: builtin!(
            "MAILBOX_MICROSOFT_CLIENT_ID",
            "MAILBOX_MICROSOFT_CLIENT_SECRET"
        ),
    },
    Provider {
        id: "yahoo",
        name: "Yahoo",
        auth_url: "https://api.login.yahoo.com/oauth2/request_auth",
        token_url: "https://api.login.yahoo.com/oauth2/get_token",
        scope: "mail-w",
        register_url: "https://developer.yahoo.com/apps/",
        builtin: builtin!("MAILBOX_YAHOO_CLIENT_ID", "MAILBOX_YAHOO_CLIENT_SECRET"),
    },
];

pub fn provider(id: &str) -> Result<&'static Provider> {
    PROVIDERS
        .iter()
        .find(|p| p.id == id.to_ascii_lowercase())
        .ok_or_else(|| {
            Error::usage(format!(
                "`{id}` is not an OAuth2 provider; known ones are {}",
                PROVIDERS
                    .iter()
                    .map(|p| p.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// The endpoints a flow will actually use.
///
/// Split out from [`Provider`] so a test — and an account at a provider the
/// table does not know — can point the flow somewhere else without the table
/// growing an entry for it.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub auth_url: String,
    pub token_url: String,
    pub scope: String,
}

impl From<&Provider> for Endpoints {
    fn from(p: &Provider) -> Endpoints {
        Endpoints {
            auth_url: p.auth_url.to_string(),
            token_url: p.token_url.to_string(),
            scope: p.scope.to_string(),
        }
    }
}

/// Which client credentials an account will use.
///
/// The account's own first, then whatever the build ships. That order is the
/// whole policy: someone who registered their own project gets their own
/// quota and their own consent screen, and everyone else gets a client that
/// works out of the box.
///
/// A pure function of its arguments — the built-in is passed in rather than
/// read here — because `option_env!` is fixed when the crate is compiled, and
/// a rule that can only be exercised in the configuration the test binary
/// happened to be built with is a rule with no tests.
pub fn choose_client(
    configured_id: Option<&str>,
    configured_secret: Option<&str>,
    builtin: Option<&BuiltinClient>,
) -> Option<Client> {
    if let Some(id) = configured_id.filter(|s| !s.is_empty()) {
        let secret = configured_secret.map(str::to_string).or_else(|| {
            // The build's secret belongs to the build's id. It applies when
            // the configured id is that same id — which is what a config
            // written before this build shipped credentials looks like — and
            // never otherwise: one application's id with another's secret is
            // an `invalid_client` nobody can debug.
            builtin
                .filter(|b| b.id == id)
                .and_then(|b| b.secret.map(str::to_string))
        });
        return Some(Client {
            id: id.to_string(),
            secret,
            builtin: false,
        });
    }
    builtin.map(|b| Client {
        id: b.id.to_string(),
        secret: b.secret.map(str::to_string),
        builtin: true,
    })
}

/// The client credentials for one account, from its config and this build.
///
/// `secret` comes from the caller because the environment overrides the config
/// for it ([`crate::credentials::client_secret`]) and this module does not read
/// the environment.
pub fn client_for(
    cfg: &crate::config::AccountConfig,
    configured_secret: Option<&str>,
) -> Option<Client> {
    let builtin = cfg
        .oauth_provider
        .as_deref()
        .and_then(|id| provider(id).ok())
        .and_then(|p| p.builtin.as_ref());
    choose_client(cfg.oauth_client_id.as_deref(), configured_secret, builtin)
}

/// Whether this build ships credentials for a provider at all.
pub fn has_builtin(provider_id: &str) -> bool {
    provider(provider_id)
        .map(|p| p.builtin.is_some())
        .unwrap_or(false)
}

/// The endpoints an account will use: the table's, with config overriding.
///
/// Overrides exist so a provider the table does not know — a self-hosted
/// identity server in front of a company's mail — is a config question rather
/// than a patch.
pub fn endpoints_for(cfg: &crate::config::AccountConfig) -> Result<Endpoints> {
    let mut endpoints = match &cfg.oauth_provider {
        Some(id) => Endpoints::from(provider(id)?),
        None => Endpoints {
            auth_url: String::new(),
            token_url: String::new(),
            scope: String::new(),
        },
    };
    if let Some(url) = &cfg.oauth_auth_url {
        endpoints.auth_url = url.clone();
    }
    if let Some(url) = &cfg.oauth_token_url {
        endpoints.token_url = url.clone();
    }
    if let Some(scope) = &cfg.oauth_scope {
        endpoints.scope = scope.clone();
    }
    if endpoints.token_url.is_empty() {
        return Err(Error::usage(
            "this account has no OAuth2 token endpoint; set oauth_provider or oauth_token_url",
        ));
    }
    Ok(endpoints)
}

/// What a completed flow produced.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tokens {
    pub access_token: String,
    /// Absent when the provider did not issue one — which for Google means the
    /// account had already granted consent, and is why `prompt=consent` is
    /// sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. Zero when the provider did not say.
    #[serde(default)]
    pub expires_at: i64,
}

impl Tokens {
    /// Whether the access token can still be used.
    ///
    /// With a minute of slack: a token that expires while the IMAP handshake
    /// is in flight fails the connection rather than the check, and the retry
    /// is a whole sync away.
    pub fn is_fresh(&self, now: i64) -> bool {
        !self.access_token.is_empty() && (self.expires_at == 0 || self.expires_at > now + 60)
    }
}

// ---- PKCE (RFC 7636) ----

/// A verifier and the challenge derived from it.
///
/// S256 only. `plain` is in the RFC and is worth nothing: the whole point is
/// that what travels through the browser cannot be replayed by whoever sees
/// it, and with `plain` it is the secret itself.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new(rng: &crate::Rng) -> Pkce {
        // 32 bytes base64url'd is 43 characters, the RFC's minimum and the
        // length every provider is known to accept.
        let verifier = base64url(&rng.bytes::<32>());
        Pkce {
            challenge: base64url(&sha256(verifier.as_bytes())),
            verifier,
        }
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---- the flow ----

/// How the login should behave, for the caller that drives it.
pub struct LoginRequest {
    pub client_id: String,
    /// Providers that still require one for an "installed app"; empty
    /// otherwise. Not a secret in any meaningful sense when it ships in a
    /// desktop binary, which is why PKCE is mandatory here regardless.
    pub client_secret: Option<String>,
    pub endpoints: Endpoints,
    /// Pre-filling the account chooser, so someone with several logins is not
    /// asked which mailbox this is about after having just typed it.
    pub login_hint: Option<String>,
    /// How long to wait for the browser to come back.
    pub timeout: Duration,
    /// Whether to hand the URL to the desktop's browser.
    ///
    /// False on a machine that has none: the URL has been announced, and the
    /// login is finished from wherever it was opened.
    pub open_browser: bool,
}

/// A loopback listener waiting for the provider's redirect.
///
/// Bound before the browser opens, because the port has to appear in the
/// `redirect_uri` — and on loopback specifically, since a redirect to anywhere
/// else would hand the authorization code to another machine.
pub struct Redirect {
    listener: TcpListener,
    port: u16,
}

impl Redirect {
    pub fn bind() -> Result<Redirect> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| Error::Network(format!("could not listen for the redirect: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Network(format!("could not read the redirect port: {e}")))?
            .port();
        Ok(Redirect { listener, port })
    }

    pub fn uri(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Waits for the browser, and returns the query parameters it carried.
    pub fn wait(&self, timeout: Duration) -> Result<Vec<(String, String)>> {
        self.listener
            .set_nonblocking(false)
            .map_err(|e| Error::Network(format!("{e}")))?;
        let deadline = std::time::Instant::now() + timeout;

        loop {
            if std::time::Instant::now() >= deadline {
                // Not `Error::Timeout`: "timed out after 300000 ms" says
                // nothing about what was being waited for, and what someone
                // needs to know here is that nothing was changed and the
                // command can simply be run again.
                return Err(Error::Auth(format!(
                    "the browser did not come back within {}s, so nothing was signed in; run the login again",
                    timeout.as_secs()
                )));
            }
            // A short accept timeout rather than a blocking one, so the
            // deadline is honoured even if nothing ever connects.
            self.listener
                .set_nonblocking(true)
                .map_err(|e| Error::Network(format!("{e}")))?;
            let accepted = self.listener.accept();
            self.listener.set_nonblocking(false).ok();

            match accepted {
                Ok((stream, _)) => {
                    if let Some(params) = handle(stream)? {
                        return Ok(params);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(Error::Network(format!("accepting the redirect: {e}"))),
            }
        }
    }
}

/// Reads one request and answers it, whatever it was.
///
/// Returns `None` for a request that is not the redirect — a browser asking
/// for `/favicon.ico` must not be mistaken for the user's answer.
fn handle(stream: TcpStream) -> Result<Option<Vec<(String, String)>>> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| Error::Network(format!("{e}")))?,
    );
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| Error::Network(format!("reading the redirect: {e}")))?;

    let target = line.split_whitespace().nth(1).unwrap_or("/");
    let params = target
        .split_once('?')
        .map(|(_, query)| parse_query(query))
        .unwrap_or_default();

    let answered = params.iter().any(|(k, _)| k == "code" || k == "error");
    let page = if answered {
        "<!doctype html><meta charset=utf-8><title>mailbox</title>\
         <body style=\"font:16px system-ui;margin:4rem auto;max-width:26rem\">\
         <h1>Signed in</h1><p>You can close this tab and go back to the terminal.</p>"
    } else {
        "<!doctype html><meta charset=utf-8><title>mailbox</title><body>"
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.flush();

    Ok(answered.then_some(params))
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
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

/// Builds the URL the browser is sent to.
pub fn authorization_url(
    req: &LoginRequest,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> String {
    let enc = crate::https::percent_encode;
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}\
         &code_challenge={}&code_challenge_method=S256&state={}",
        req.endpoints.auth_url,
        enc(&req.client_id),
        enc(redirect_uri),
        enc(&req.endpoints.scope),
        enc(&pkce.challenge),
        enc(state),
    );
    // Google returns a refresh token only on the first consent unless asked
    // again; without these two an account that has been added before comes
    // back with an access token that expires in an hour and nothing to renew
    // it with.
    url.push_str("&access_type=offline&prompt=consent");
    if let Some(hint) = &req.login_hint {
        url.push_str(&format!("&login_hint={}", enc(hint)));
    }
    url
}

/// Runs the whole login: opens the browser, waits, and exchanges the code.
///
/// `announce` is handed the URL before the browser is opened, so a caller can
/// print it — a login on a machine with no browser, over SSH, is finished by
/// pasting that URL somewhere that has one.
pub fn login(
    req: &LoginRequest,
    rng: &crate::Rng,
    clock: &crate::Clock,
    announce: impl FnOnce(&str),
) -> Result<Tokens> {
    let redirect = Redirect::bind()?;
    let pkce = Pkce::new(rng);
    let state = base64url(&rng.bytes::<16>());
    let url = authorization_url(req, &redirect.uri(), &pkce, &state);

    announce(&url);
    if req.open_browser {
        // Failing to open one is not fatal: the URL has been announced, and on
        // a headless machine that was always going to be how this worked.
        let _ = crate::open::open_url(&url);
    }

    let params = redirect.wait(req.timeout)?;
    let get = |name: &str| {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };

    if let Some(error) = get("error") {
        let detail = get("error_description").unwrap_or_default();
        return Err(Error::Auth(format!(
            "the provider refused the login: {error}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        )));
    }

    // The state check is what stops a page the user visits later from
    // completing a login of its own choosing against this listener.
    match get("state") {
        Some(returned) if returned == state => {}
        _ => {
            return Err(Error::Auth(
                "the redirect carried the wrong `state`; the login was not completed".into(),
            ))
        }
    }

    let code = get("code").ok_or_else(|| Error::Auth("the redirect carried no code".into()))?;
    exchange(req, &redirect.uri(), &code, &pkce.verifier, clock)
}

/// Trades an authorization code for tokens.
fn exchange(
    req: &LoginRequest,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
    clock: &crate::Clock,
) -> Result<Tokens> {
    let mut fields = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", req.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = &req.client_secret {
        fields.push(("client_secret", secret.as_str()));
    }
    let response = crate::https::post_form(&req.endpoints.token_url, &fields)?;
    tokens_from(&response, clock, None)
}

/// Trades a refresh token for a new access token.
///
/// Providers differ on whether a new refresh token comes back; the old one is
/// kept when none does, which is what `previous` is for.
pub fn refresh(
    token_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    refresh_token: &str,
    clock: &crate::Clock,
) -> Result<Tokens> {
    let mut fields = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = client_secret {
        fields.push(("client_secret", secret));
    }
    let response = crate::https::post_form(token_url, &fields)?;
    tokens_from(&response, clock, Some(refresh_token))
}

fn tokens_from(
    response: &crate::https::Response,
    clock: &crate::Clock,
    previous_refresh: Option<&str>,
) -> Result<Tokens> {
    let body = response.json()?;
    if response.status != 200 {
        // The provider's own words: `invalid_grant` means the refresh token
        // was revoked, and telling someone "authentication failed" when their
        // answer is to log in again is not help.
        let code = body
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("the request was refused");
        let detail = body
            .get("error_description")
            .and_then(|e| e.as_str())
            .unwrap_or_default();
        return Err(Error::Auth(format!(
            "the token endpoint refused this ({}): {code}{}",
            response.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        )));
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Auth("the token response carried no access_token".into()))?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| previous_refresh.map(str::to_string));
    let expires_at = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .map(|secs| clock.now() + secs)
        .unwrap_or(0);

    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

// ---- the cache ----

/// Where an account's access token is cached.
fn token_path(account_id: &str) -> PathBuf {
    crate::paths::account_dir(account_id).join("oauth.json")
}

/// Reads the cached tokens, if any. A missing or unreadable file is `None`:
/// the cache is an optimisation, and a corrupt one means logging in again
/// rather than failing.
pub fn cached(account_id: &str) -> Option<Tokens> {
    let path = token_path(account_id);
    let root = path.parent()?;
    let key = path.file_name().map(PathBuf::from)?;
    cached_in(&FsStorage::new(root), &key)
}

pub fn cached_in(storage: &dyn ByteStorage, key: &std::path::Path) -> Option<Tokens> {
    let bytes = storage.read(key).ok()??;
    serde_json::from_slice(&bytes).ok()
}

/// Writes the cache, readable only by this user.
pub fn cache(account_id: &str, tokens: &Tokens) -> Result<()> {
    let path = token_path(account_id);
    let root = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let key = path
        .file_name()
        .map(PathBuf::from)
        .ok_or_else(|| Error::store(format!("invalid OAuth token path `{}`", path.display())))?;
    cache_in(&FsStorage::new(root), &key, tokens)
}

pub fn cache_in(storage: &dyn ByteStorage, key: &std::path::Path, tokens: &Tokens) -> Result<()> {
    storage.write(key, serde_json::to_string(tokens)?.as_bytes())?;
    Ok(())
}

pub fn forget_tokens(account_id: &str) {
    let path = token_path(account_id);
    let Some(root) = path.parent() else { return };
    let Some(key) = path.file_name().map(PathBuf::from) else {
        return;
    };
    forget_tokens_in(&FsStorage::new(root), &key);
}

pub fn forget_tokens_in(storage: &dyn ByteStorage, key: &std::path::Path) {
    let _ = storage.remove(key);
}

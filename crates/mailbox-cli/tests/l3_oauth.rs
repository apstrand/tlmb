//! **L3** (§14.2): OAuth2, against an authorization server that misbehaves.
//!
//! The checks that matter in a token flow are the ones a cooperative server
//! never exercises. A client that ignores `state`, or sends a PKCE verifier it
//! made up, or treats a captive portal's HTML as a token, works perfectly right
//! up until it does not — and the failure is somebody else's mailbox.

use std::time::Duration;

use mailbox_core::clock::{Clock, Rng};
use mailbox_core::oauth::{self, Endpoints, LoginRequest, Pkce, Redirect, Tokens};
use mailbox_l3::{OauthFault, OauthServer};

fn request(server: &OauthServer) -> LoginRequest {
    LoginRequest {
        client_id: "test-client".into(),
        client_secret: None,
        endpoints: Endpoints {
            auth_url: server.auth_url(),
            token_url: server.token_url(),
            scope: "mail".into(),
        },
        login_hint: Some("peter@example.com".into()),
        timeout: Duration::from_secs(10),
        // The tests are the browser; opening a real one would put a consent
        // screen on the developer's desktop during `cargo test`.
        open_browser: false,
    }
}

/// Drives the browser half: reads the URL the flow printed, and fetches the
/// redirect the provider would have sent, optionally mangled.
fn browser(url: String, mangle: impl Fn(&mut Vec<(String, String)>) + Send + 'static) {
    std::thread::spawn(move || {
        let query = url
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .to_string();
        let params: Vec<(String, String)> = query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let get = |name: &str| {
            params
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };

        let redirect = percent_decode(&get("redirect_uri"));
        let mut back = vec![
            ("code".to_string(), "the-code".to_string()),
            ("state".to_string(), get("state")),
        ];
        mangle(&mut back);

        let query = back
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        // A plain GET, which is all a browser following a 302 does.
        let _ = fetch(&format!("{redirect}?{query}"));
    });
}

fn fetch(url: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = std::net::TcpStream::connect(authority)?;
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )?;
    let mut body = String::new();
    stream.read_to_string(&mut body)?;
    Ok(body)
}

fn percent_decode(s: &str) -> String {
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Runs a login, standing in for the browser, and returns what came back.
fn login_with(
    server: &OauthServer,
    mangle: impl Fn(&mut Vec<(String, String)>) + Send + 'static,
) -> mailbox_core::Result<Tokens> {
    login_against(server, mangle, true)
}

/// `honest_challenge = false` registers the code against a challenge the flow
/// did not send, which is what a stolen authorization code looks like to the
/// provider.
fn login_against(
    server: &OauthServer,
    mangle: impl Fn(&mut Vec<(String, String)>) + Send + 'static,
    honest_challenge: bool,
) -> mailbox_core::Result<Tokens> {
    let req = request(server);
    let rng = Rng::seeded(b"oauth-test");
    let clock = Clock::fixed(1_788_000_000);

    // The code has to be registered against the challenge the flow generated,
    // which is only visible in the URL — so the server is told from inside the
    // announce callback, before the browser is let loose.
    oauth::login(&req, &rng, &clock, |url| {
        let challenge = url
            .split('&')
            .find_map(|p| p.strip_prefix("code_challenge="))
            .unwrap_or_default()
            .to_string();
        server.issue_code(
            "the-code",
            if honest_challenge {
                &challenge
            } else {
                "a-challenge-from-somewhere-else"
            },
        );
        browser(url.to_string(), mangle);
    })
}

// ---- the happy path ----

#[test]
fn a_login_exchanges_the_code_for_tokens() {
    let server = OauthServer::start().expect("start the L3 authorization server");
    let tokens = login_with(&server, |_| {}).expect("the login should succeed");

    assert_eq!(tokens.access_token, "access-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    assert!(
        tokens.expires_at > 1_788_000_000,
        "an expiry should have been derived from expires_in: {tokens:?}"
    );

    let sent = server.requests();
    let exchange = sent.last().expect("a token request");
    assert_eq!(exchange.get("grant_type"), Some("authorization_code"));
    assert_eq!(exchange.get("client_id"), Some("test-client"));
    // The verifier, not the challenge: sending the challenge would authenticate
    // anyone who watched the browser.
    let verifier = exchange.get("code_verifier").expect("a verifier");
    assert!(verifier.len() >= 43, "RFC 7636's minimum: {verifier}");
    assert!(exchange
        .get("redirect_uri")
        .unwrap()
        .starts_with("http://127.0.0.1:"));
}

/// The authorization URL has to carry everything the provider needs, and one
/// thing it does not require but Google needs asking twice for.
#[test]
fn the_authorization_url_asks_for_a_refresh_token() {
    let server = OauthServer::start().expect("start");
    let req = request(&server);
    let rng = Rng::seeded(b"url");
    let pkce = Pkce::new(&rng);
    let url = oauth::authorization_url(&req, "http://127.0.0.1:9/", &pkce, "st4te");

    assert!(url.contains("response_type=code"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
    assert!(url.contains("state=st4te"));
    assert!(
        url.contains("access_type=offline") && url.contains("prompt=consent"),
        "without these Google issues no refresh token on a repeat login: {url}"
    );
    assert!(
        url.contains("login_hint=peter%40example.com"),
        "the hint should be encoded, not raw: {url}"
    );
    assert!(
        !url.contains(&pkce.verifier),
        "the verifier must never reach the browser: {url}"
    );
}

// ---- what a hostile or broken redirect does ----

#[test]
fn a_redirect_with_the_wrong_state_is_refused() {
    let server = OauthServer::start().expect("start");
    let error = login_with(&server, |params| {
        for (k, v) in params.iter_mut() {
            if k == "state" {
                *v = "not-the-state".into();
            }
        }
    })
    .expect_err("a mismatched state must not complete a login");

    assert!(
        error.to_string().contains("state"),
        "and it should say why: {error}"
    );
    assert!(
        server.requests().is_empty(),
        "nothing should have been sent to the token endpoint"
    );
}

#[test]
fn a_redirect_carrying_an_error_is_reported_with_the_providers_words() {
    let server = OauthServer::start().expect("start");
    let error = login_with(&server, |params| {
        params.clear();
        params.push(("error".into(), "access_denied".into()));
        params.push(("error_description".into(), "The+user+said+no".into()));
    })
    .expect_err("a denied consent is not a success");

    let message = error.to_string();
    assert!(message.contains("access_denied"), "{message}");
    assert!(message.contains("said no"), "{message}");
}

// ---- what the token endpoint can do to us ----

/// PKCE, from the provider's side.
///
/// This is as much a test of the L3 server as of the client: if it accepted
/// any verifier at all, every test above would pass with a client that sent
/// garbage, and the one property PKCE exists to give — that a code stolen out
/// of the browser is useless without the verifier that never left this
/// process — would be untested.
#[test]
fn a_code_issued_against_another_challenge_cannot_be_redeemed() {
    let server = OauthServer::start().expect("start");
    let error = login_against(&server, |_| {}, false)
        .expect_err("a verifier that does not match the challenge must not yield a token");

    let message = error.to_string();
    assert!(message.contains("invalid_grant"), "{message}");
    assert!(message.contains("code_verifier"), "{message}");
}

#[test]
fn a_revoked_refresh_token_says_so_rather_than_failing_vaguely() {
    let server = OauthServer::start().expect("start");
    server.set_fault(OauthFault::InvalidGrant);

    let error = mailbox_core::oauth::refresh(
        &server.token_url(),
        "test-client",
        None,
        "revoked",
        &Clock::fixed(0),
    )
    .expect_err("a revoked token cannot be refreshed");

    let message = error.to_string();
    assert!(
        message.contains("invalid_grant"),
        "the provider's own code is what tells the user to log in again: {message}"
    );
    assert!(message.contains("revoked"), "{message}");
}

#[test]
fn a_refresh_keeps_the_old_refresh_token_when_none_comes_back() {
    let server = OauthServer::start().expect("start");
    server.set_fault(OauthFault::NoRefreshToken);

    let tokens = mailbox_core::oauth::refresh(
        &server.token_url(),
        "test-client",
        None,
        "the-original",
        &Clock::fixed(0),
    )
    .expect("the refresh itself succeeds");

    assert_eq!(
        tokens.refresh_token.as_deref(),
        Some("the-original"),
        "dropping it would make the next refresh impossible"
    );
}

#[test]
fn a_response_that_is_not_json_is_an_error_and_not_a_token() {
    let server = OauthServer::start().expect("start");
    server.set_fault(OauthFault::NotJson);

    let error = mailbox_core::oauth::refresh(
        &server.token_url(),
        "test-client",
        None,
        "whatever",
        &Clock::fixed(0),
    )
    .expect_err("HTML is not a token");
    assert!(error.to_string().contains("not JSON"), "{error}");
}

#[test]
fn an_already_expired_token_is_not_treated_as_fresh() {
    let server = OauthServer::start().expect("start");
    server.set_fault(OauthFault::ExpiredToken);
    let clock = Clock::fixed(1_788_000_000);

    let tokens =
        mailbox_core::oauth::refresh(&server.token_url(), "test-client", None, "whatever", &clock)
            .expect("the exchange succeeds");

    assert!(
        !tokens.is_fresh(clock.now()),
        "a token that expires now must not be used: {tokens:?}"
    );
}

/// The slack in `is_fresh`: a token with seconds left is not worth starting a
/// connection with, because it will expire during the handshake.
#[test]
fn a_token_about_to_expire_is_refreshed_early() {
    let now = 1_788_000_000;
    let almost = Tokens {
        access_token: "a".into(),
        refresh_token: None,
        expires_at: now + 30,
    };
    assert!(!almost.is_fresh(now));

    let plenty = Tokens {
        access_token: "a".into(),
        refresh_token: None,
        expires_at: now + 600,
    };
    assert!(plenty.is_fresh(now));

    // A provider that said nothing about expiry: use it and find out.
    let unknown = Tokens {
        access_token: "a".into(),
        refresh_token: None,
        expires_at: 0,
    };
    assert!(unknown.is_fresh(now));
}

// ---- the redirect listener ----

#[test]
fn the_redirect_listens_on_loopback_only() {
    let redirect = Redirect::bind().expect("bind");
    let uri = redirect.uri();
    assert!(
        uri.starts_with("http://127.0.0.1:"),
        "a redirect anywhere else would hand the code to another machine: {uri}"
    );
}

#[test]
fn a_stray_request_is_not_mistaken_for_the_answer() {
    let redirect = Redirect::bind().expect("bind");
    let uri = redirect.uri();

    std::thread::spawn(move || {
        // What a browser actually does: asks for the favicon too.
        let _ = fetch(&format!("{uri}favicon.ico"));
        std::thread::sleep(Duration::from_millis(100));
        let _ = fetch(&format!("{uri}?code=real&state=s"));
    });

    let params = redirect
        .wait(Duration::from_secs(10))
        .expect("the real redirect should arrive");
    assert_eq!(
        params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.as_str()),
        Some("real")
    );
}

// ---- the whole thing, through the binary ----

mod cli {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct World {
        dir: PathBuf,
        config: PathBuf,
    }

    impl World {
        fn new(server: &OauthServer) -> World {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("mailbox-oauth-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            let config = dir.join("config.toml");
            // The endpoint overrides are what let an account point at a
            // provider the built-in table has never heard of — here, one on
            // loopback with no certificate.
            std::fs::write(
                &config,
                format!(
                    "[accounts.work]\n\
                     address = \"peter@example.com\"\n\
                     imap_host = \"imap.example.com\"\n\
                     oauth_provider = \"google\"\n\
                     oauth_client_id = \"test-client\"\n\
                     oauth_auth_url = \"{}\"\n\
                     oauth_token_url = \"{}\"\n\
                     oauth_scope = \"mail\"\n",
                    server.auth_url(),
                    server.token_url()
                ),
            )
            .expect("write config");
            World { config, dir }
        }

        fn command(&self, args: &[&str]) -> Command {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
            cmd.args(args)
                .env("MAILBOX_CONFIG", &self.config)
                .env("MAILBOX_STATE_DIR", self.dir.join("state"))
                .env("XDG_RUNTIME_DIR", &self.dir)
                .env("NO_COLOR", "1")
                // Nothing here may put a popup on the desktop of
                // whoever ran the suite (§11).
                .env("MAILBOX_NOTIFIER", "none")
                // Nor may it resolve or connect to whatever domain a
                // fixture happens to name (§9 discovery).
                .env("MAILBOX_DISCOVERY", "off")
                // Nor may it write a fixture password into the keyring of
                // whoever ran `cargo test`.
                .env("MAILBOX_KEYCHAIN", "none")
                .env_remove("MAILBOX_AGENT");
            cmd
        }

        fn token_file(&self) -> PathBuf {
            self.dir.join("state").join("work").join("oauth.json")
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Runs `accounts login --no-browser`, plays the browser, and returns the
    /// command's own JSON document.
    fn login(world: &World, server: &OauthServer) -> serde_json::Value {
        let mut child = world
            .command(&[
                "--json",
                "accounts",
                "login",
                "work",
                "--no-browser",
                "--wait",
                "20",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        // The URL goes to stderr precisely so that stdout can stay the one
        // JSON document of §6.4 — which this reads back below.
        let mut err = BufReader::new(child.stderr.take().expect("stderr"));
        let mut url = String::new();
        err.read_line(&mut url).expect("the login URL");
        let url = url.trim().to_string();
        assert!(url.starts_with(&server.auth_url()), "unexpected URL: {url}");

        let challenge = url
            .split('&')
            .find_map(|p| p.strip_prefix("code_challenge="))
            .expect("a challenge")
            .to_string();
        server.issue_code("the-code", &challenge);
        browser(url, |_| {});

        let out = child.wait_with_output().expect("wait");
        assert!(
            out.status.success(),
            "login failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "expected one JSON document, got {e}: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// The regression: an account carrying `oauth_client_id` — written before
    /// this build shipped credentials, or by someone following the older
    /// instructions — must still send the secret that goes with that id.
    ///
    /// Google's answer when it does not is `invalid_request — client_secret is
    /// missing`, which says nothing about the config line responsible.
    #[test]
    fn a_configured_client_id_still_sends_a_secret() {
        let server = OauthServer::start().expect("start");
        let world = World::new(&server);

        // As `accounts add --oauth-client-id` writes it.
        let config = std::fs::read_to_string(&world.config).unwrap();
        std::fs::write(
            &world.config,
            format!("{config}oauth_client_secret = \"configured-secret\"\n"),
        )
        .unwrap();

        login(&world, &server);
        let sent = server.requests();
        let exchange = sent.last().expect("a token request");
        assert_eq!(
            exchange.get("client_secret"),
            Some("configured-secret"),
            "the token request went out without a secret: {:?}",
            exchange.fields
        );
    }

    #[test]
    fn logging_in_stores_a_token_the_next_command_can_use() {
        let server = OauthServer::start().expect("start");
        let world = World::new(&server);

        let doc = login(&world, &server);
        assert_eq!(doc["account"], "work");
        assert_eq!(doc["refresh_token"], true);

        // §9: the token is a file of its own, not the config and not meta.db.
        let cached = std::fs::read_to_string(world.token_file()).expect("a token file");
        assert!(cached.contains("access-1"), "{cached}");
        assert!(
            !std::fs::read_to_string(&world.config)
                .unwrap()
                .contains("access-1"),
            "the config must never hold a token"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(world.token_file())
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "a bearer token must not be readable by anyone else: {mode:o}"
            );
        }
    }

    /// The refresh that makes the whole thing usable: a command run an hour
    /// later gets a new access token without anyone opening a browser.
    #[test]
    fn an_expired_access_token_is_refreshed_without_a_browser() {
        let server = OauthServer::start().expect("start");
        let world = World::new(&server);
        login(&world, &server);

        // Age the cached token past its expiry, as an hour would.
        let path = world.token_file();
        let mut tokens: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        tokens["expires_at"] = serde_json::json!(1);
        std::fs::write(&path, tokens.to_string()).unwrap();

        // `accounts test` connects, so it fails — but only after resolving a
        // credential, which is the part under test. The refresh is visible in
        // what the authorization server was asked for.
        let before = server.requests().len();
        let _ = world
            .command(&["--json", "accounts", "test", "work"])
            .output();

        let sent = server.requests();
        assert!(sent.len() > before, "no refresh was attempted");
        let refresh = sent.last().unwrap();
        assert_eq!(refresh.get("grant_type"), Some("refresh_token"));
        assert_eq!(refresh.get("refresh_token"), Some("refresh-1"));

        let cached: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            cached["access_token"], "access-2",
            "the new token should have replaced the stale one: {cached}"
        );
    }

    /// A revoked refresh token has one answer, and the error has to say it.
    #[test]
    fn a_revoked_login_tells_the_user_to_log_in_again() {
        let server = OauthServer::start().expect("start");
        let world = World::new(&server);
        login(&world, &server);

        let path = world.token_file();
        let mut tokens: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        tokens["expires_at"] = serde_json::json!(1);
        std::fs::write(&path, tokens.to_string()).unwrap();
        server.set_fault(OauthFault::InvalidGrant);

        let out = world
            .command(&["--json", "accounts", "test", "work"])
            .output()
            .expect("run");
        assert_eq!(out.status.code(), Some(4), "§6.6's auth code");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("invalid_grant") || stderr.contains("revoked"),
            "{stderr}"
        );
    }
}

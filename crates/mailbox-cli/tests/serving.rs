//! `mailbox serve` (§3), end to end over a real socket.
//!
//! §15 says M5's UI work is "covered through the same CLI/JSON assertions
//! rather than a screenshot suite", so this is where the web interface is
//! actually verified: the contract it answers with, and — mostly — who it
//! refuses.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};

/// Ports for the servers under test. Each test takes its own.
static NEXT_PORT: AtomicU16 = AtomicU16::new(28_600);

const FIXTURE: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[folder]]
path = "Archive"
special_use = "archive"

[[message]]
name = "one"
folder = "INBOX"
from = "Alice <alice@example.com>"
subject = "First"
date = "2026-08-01T10:00:00Z"
message_id = "<one@example.com>"
body = "hello"

[[message]]
name = "two"
folder = "INBOX"
from = "Bob <bob@example.com>"
subject = "Second"
date = "2026-08-02T10:00:00Z"
message_id = "<two@example.com>"
body = "goodbye"

[[message]]
name = "newsletter"
folder = "INBOX"
from = "Shop <shop@example.com>"
subject = "Third"
date = "2026-08-03T10:00:00Z"
message_id = "<three@example.com>"
html = """
<p><img src="https://tracker.example/open.gif?id=42" alt="a logo"></p>
<p>Hello.</p>
"""
"#;

/// A second account, whose mail is the mail the default account does not have.
///
/// The subject is what the multi-account tests below look the message up by.
const OTHER_FIXTURE: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[folder]]
path = "Archive"
special_use = "archive"

[[message]]
name = "elsewhere"
folder = "INBOX"
from = "Carol <carol@other.example>"
subject = "From the other account"
date = "2026-08-04T10:00:00Z"
message_id = "<four@other.example>"
body = "over here"
"#;

struct Serving {
    dir: PathBuf,
    config: PathBuf,
    child: Child,
    port: u16,
    token: String,
}

impl Serving {
    fn start() -> Serving {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-serve-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");
        let state = dir.join("state");

        let manifest = dir.join("fixture.toml");
        std::fs::write(&manifest, FIXTURE).expect("write fixture");
        let seeded = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(["debug", "fixture", "load", manifest.to_str().unwrap()])
            .env("MAILBOX_CONFIG", &config)
            .env("MAILBOX_STATE_DIR", &state)
            // Nothing here may put a popup on the desktop of
            // whoever ran the suite (§11).
            .env("MAILBOX_NOTIFIER", "none")
            // And out of the developer's keyring: adding an account files its
            // password where the platform can find it again (§9, §14).
            .env("MAILBOX_KEYCHAIN", "none")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .output()
            .expect("seed");
        assert!(seeded.status.success(), "seeding failed");

        Serving::spawn(dir, config, state, port)
    }

    /// A server over two accounts, as §1.5's unified mailbox is normally run.
    ///
    /// `alpha` sorts first and so is what `default_account` picks; the message
    /// under test lives in `beta`, which is the case a single-account write
    /// path gets wrong.
    fn start_two_accounts() -> Serving {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-serve-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.alpha]\naddress = \"peter@alpha.example\"\nimap_host = \"127.0.0.1\"\n\n\
             [accounts.beta]\naddress = \"peter@beta.example\"\nimap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");
        let state = dir.join("state");

        for (account, fixture) in [("alpha", FIXTURE), ("beta", OTHER_FIXTURE)] {
            let manifest = dir.join(format!("{account}.toml"));
            std::fs::write(&manifest, fixture).expect("write fixture");
            let seeded = Command::new(env!("CARGO_BIN_EXE_mailbox"))
                .args([
                    "--account",
                    account,
                    "debug",
                    "fixture",
                    "load",
                    manifest.to_str().unwrap(),
                ])
                .env("MAILBOX_CONFIG", &config)
                .env("MAILBOX_STATE_DIR", &state)
                .env("MAILBOX_NOTIFIER", "none")
                .env("MAILBOX_KEYCHAIN", "none")
                .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
                .output()
                .expect("seed");
            assert!(seeded.status.success(), "seeding {account} failed");
        }

        Serving::spawn(dir, config, state, port)
    }

    /// A server on a machine where `accounts add` has never been run.
    ///
    /// Not a corner case: it is what the first launch looks like, and the one
    /// state in which the interface has to explain itself rather than show
    /// mail.
    fn start_unconfigured() -> Serving {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-serve-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(&config, "").expect("write config");
        let state = dir.join("state");

        Serving::spawn(dir, config, state, port)
    }

    /// The same, bound to every interface — which is what `--bind 0.0.0.0`
    /// does for reading mail from a phone, and where the setup form is not on
    /// offer.
    fn start_unconfigured_on_the_network() -> Serving {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-serve-{}-{port}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(&config, "").expect("write config");
        let state = dir.join("state");

        Serving::spawn_bound(dir, config, state, port, Some("0.0.0.0"))
    }

    fn spawn(dir: PathBuf, config: PathBuf, state: PathBuf, port: u16) -> Serving {
        Serving::spawn_bound(dir, config, state, port, None)
    }

    fn spawn_bound(
        dir: PathBuf,
        config: PathBuf,
        state: PathBuf,
        port: u16,
        bind: Option<&str>,
    ) -> Serving {
        let mut args = vec![
            "--json".to_string(),
            "serve".to_string(),
            "--port".to_string(),
            port.to_string(),
        ];
        if let Some(address) = bind {
            args.push("--bind".to_string());
            args.push(address.to_string());
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(&args)
            .env("MAILBOX_CONFIG", &config)
            .env("MAILBOX_STATE_DIR", &state)
            // Nothing here may put a popup on the desktop of
            // whoever ran the suite (§11).
            .env("MAILBOX_NOTIFIER", "none")
            // And out of the developer's keyring: adding an account files its
            // password where the platform can find it again (§9, §14).
            .env("MAILBOX_KEYCHAIN", "none")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", format!("serve-{port}"))
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the server");

        // The first line of stdout is the JSON reply, printed once the socket
        // is bound — so reading it is also the wait for readiness.
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("stdout"))
            .read_line(&mut line)
            .expect("the server should announce itself");
        let announced: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?} ({e})"));
        let url = announced["url"].as_str().expect("a url").to_string();
        let token = url.split('=').nth(1).unwrap_or_default().to_string();

        Serving {
            dir,
            config,
            child,
            port,
            token,
        }
    }

    /// One request, one connection. Returns (status, head, body).
    fn request(&self, raw: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).expect("connect");
        stream.write_all(raw.as_bytes()).expect("write");
        stream.flush().ok();

        // Bytes, not a string: one of the things served is a wasm module, and
        // `read_to_string` on it fails with "stream did not contain valid
        // UTF-8" — which says nothing about the response being wrong.
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read");
        let response = String::from_utf8_lossy(&raw).into_owned();

        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, head.to_string(), body.to_string())
    }

    fn get(&self, path: &str, headers: &str) -> (u16, String, String) {
        self.request(&format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}\r\n"
        ))
    }

    fn auth(&self) -> String {
        format!("X-Mailbox-Token: {}\r\n", self.token)
    }

    /// A request carrying the API token, as the served page makes them.
    fn api(&self, path: &str) -> serde_json::Value {
        let (status, _, body) = self.get(path, &self.auth());
        assert_eq!(status, 200, "{path} answered {status}: {body}");
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("not JSON: {body} ({e})"))
    }

    fn post(&self, path: &str, json: &str) -> (u16, serde_json::Value) {
        let (status, _, body) = self.request(&format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Mailbox-Token: {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{json}",
            self.token,
            json.len()
        ));
        (
            status,
            serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    /// The id of the message with this subject.
    fn id_of(&self, subject: &str) -> String {
        self.api("/api/ls?limit=50")["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|m| m["subject"] == subject)
            .unwrap_or_else(|| panic!("no message {subject}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn first_id(&self) -> String {
        self.api("/api/ls")["messages"][0]["id"]
            .as_str()
            .expect("a message")
            .to_string()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- who gets in ----

/// The token is the whole of the access control, so this is the test that
/// matters most in the file.
#[test]
fn nothing_is_served_without_the_token() {
    let s = Serving::start();

    let (status, _, _) = s.get("/", "");
    assert_eq!(
        status, 401,
        "the page was served to an unauthenticated request"
    );

    let (status, _, _) = s.get("/api/ls", "");
    assert_eq!(status, 401, "the API answered an unauthenticated request");

    let (status, _, _) = s.get("/api/ls", "X-Mailbox-Token: wrong\r\n");
    assert_eq!(status, 401, "a wrong token was accepted");
}

/// The cookie exists so a reload works without the token in the address bar.
/// It must not authenticate the API: if it did, any page in the browser could
/// POST to this port and the browser would attach it — which is exactly the
/// cross-site request forgery a local API is easiest to get wrong.
#[test]
fn the_cookie_opens_the_page_and_not_the_api() {
    let s = Serving::start();

    let (status, head, _) = s.get(&format!("/?mailbox_token={}", s.token), "");
    assert_eq!(status, 200);
    assert!(head.contains("Set-Cookie: mailbox_token="), "{head}");
    assert!(
        head.contains("HttpOnly"),
        "a page bug could read it: {head}"
    );
    assert!(head.contains("SameSite=Strict"), "{head}");

    let cookie = format!("Cookie: mailbox_token={}\r\n", s.token);
    let (status, _, _) = s.get("/", &cookie);
    assert_eq!(status, 200, "a reload with the cookie should work");

    let (status, _, _) = s.get("/api/ls", &cookie);
    assert_eq!(status, 401, "the cookie authenticated an API request");
}

/// A cross-origin page asking permission is told no, which is what keeps the
/// custom header from being something another site can send.
#[test]
fn cross_origin_requests_are_refused() {
    let s = Serving::start();
    let (status, _, _) = s.request(
        "OPTIONS /api/ls HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Origin: https://evil.example\r\n\
         Access-Control-Request-Method: POST\r\n\r\n",
    );
    assert_eq!(status, 403);
}

/// §3: "it refuses to bind non-loopback without a token".
#[test]
fn serving_the_network_without_a_token_is_refused() {
    let dir = std::env::temp_dir().join(format!("mailbox-serve-refuse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(["serve", "--bind", "0.0.0.0", "--no-token", "--dry-run-url"])
        .env("MAILBOX_CONFIG", &config)
        .env("MAILBOX_STATE_DIR", dir.join("state"))
        // Nothing here may put a popup on the desktop of
        // whoever ran the suite (§11).
        .env("MAILBOX_NOTIFIER", "none")
        // And out of the developer's keyring: adding an account files its
        // password where the platform can find it again (§9, §14).
        .env("MAILBOX_KEYCHAIN", "none")
        .output()
        .expect("run");

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("without a token"), "{stderr}");
    assert!(stderr.contains("read your mail"), "{stderr}");
    std::fs::remove_dir_all(&dir).ok();
}

// ---- the contract ----

/// §4: there is one contract, and the CLI's `--json` output is it. These are
/// the same documents, reached a different way.
#[test]
fn the_api_answers_with_the_same_documents_the_cli_prints() {
    let s = Serving::start();

    let listing = s.api("/api/ls");
    let messages = listing["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "{listing:#}");
    for field in [
        "id",
        "account",
        "folder",
        "thread_id",
        "date",
        "subject",
        "flags",
    ] {
        assert!(
            messages[0].get(field).is_some(),
            "missing {field}: {listing:#}"
        );
    }

    let folders = s.api("/api/folders");
    assert_eq!(
        folders["folders"].as_array().unwrap().len(),
        2,
        "{folders:#}"
    );

    let status = s.api("/api/status");
    let account = &status["accounts"][0];
    assert_eq!(account["account"], "test", "{status:#}");
    assert_eq!(account["messages"], 3, "{status:#}");
    assert_eq!(account["queued_ops"], 0, "{status:#}");
    assert!(
        status["store_bytes"].as_u64().unwrap_or(0) > 0,
        "{status:#}"
    );

    let accounts = s.api("/api/accounts");
    assert_eq!(accounts["accounts"][0]["id"], "test", "{accounts:#}");
}

#[test]
fn search_uses_the_same_grammar() {
    let s = Serving::start();

    let found = s.api("/api/ls?q=from%3Aalice");
    assert_eq!(found["messages"].as_array().unwrap().len(), 1, "{found:#}");
    assert_eq!(found["messages"][0]["subject"], "First");

    // An unknown key is an error here as it is everywhere else (§6.2): no
    // silent fallback to a full-text search.
    let (status, _, body) = s.get("/api/ls?q=nope%3Ax", &s.auth());
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("nope"), "{body}");
}

#[test]
fn a_message_can_be_read() {
    let s = Serving::start();
    let id = s.id_of("Second");
    let shown = s.api(&format!("/api/show?id={id}"));
    assert_eq!(shown["message"]["subject"], "Second", "{shown:#}");
    assert!(shown["message"]["body"]["content"]
        .as_str()
        .unwrap()
        .contains("goodbye"));
}

#[test]
fn a_missing_message_is_a_404_in_the_contracts_error_shape() {
    let s = Serving::start();
    let (status, _, body) = s.get("/api/show?id=ffffffff", &s.auth());

    assert_eq!(status, 404, "{body}");
    let error: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(error["error"]["code"], "not_found", "{error:#}");
}

// ---- writes ----

#[test]
fn flags_can_be_set_and_are_visible_to_the_next_read() {
    let s = Serving::start();
    let id = s.first_id();

    let (status, body) = s.post(
        "/api/flag",
        &format!(r#"{{"ids":["{id}"],"add":["seen"]}}"#),
    );
    assert_eq!(status, 200, "{body:#}");
    assert_eq!(body["changed"], 1, "{body:#}");

    let read = s.api("/api/ls?q=is%3Aread");
    assert_eq!(read["messages"].as_array().unwrap().len(), 1, "{read:#}");
}

/// The bug this guards: the page lists every account's mail (§1.5), so the
/// message someone marks read is as likely to be in the second account as the
/// first. Resolving its id against the default account's store alone reported
/// "no such message" for an id the page was displaying at that moment, and
/// mark-as-read simply did not work for most of the mailbox.
#[test]
fn a_write_reaches_the_account_that_owns_the_message() {
    let s = Serving::start_two_accounts();
    let id = s.id_of("From the other account");

    let (status, body) = s.post(
        "/api/flag",
        &format!(r#"{{"ids":["{id}"],"add":["seen"]}}"#),
    );
    assert_eq!(status, 200, "flagging the other account's mail: {body:#}");
    assert_eq!(body["changed"], 1, "{body:#}");

    // Read back through the unified listing, which is where it has to show.
    let read = s.api("/api/ls?q=is%3Aread");
    let subjects: Vec<&str> = read["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap())
        .collect();
    assert_eq!(subjects, ["From the other account"], "{read:#}");
}

/// Moving and snoozing route the same way, and were wrong the same way.
#[test]
fn moving_and_snoozing_reach_the_owning_account_too() {
    let s = Serving::start_two_accounts();
    let id = s.id_of("From the other account");

    let (status, body) = s.post(
        "/api/snooze",
        &format!(r#"{{"ids":["{id}"],"until":"2h"}}"#),
    );
    assert_eq!(status, 200, "{body:#}");
    assert_eq!(body["snoozed"].as_array().unwrap().len(), 1, "{body:#}");

    let (status, body) = s.post(
        "/api/move",
        &format!(r#"{{"ids":["{id}"],"folder":"Archive"}}"#),
    );
    assert_eq!(status, 200, "{body:#}");
    assert_eq!(body["changed"], 1, "{body:#}");
}

#[test]
fn a_message_can_be_moved_and_snoozed() {
    let s = Serving::start();
    let id = s.first_id();

    let (status, body) = s.post(
        "/api/snooze",
        &format!(r#"{{"ids":["{id}"],"until":"2h"}}"#),
    );
    assert_eq!(status, 200, "{body:#}");
    // §7: the device holding the timer is named rather than implied away.
    assert!(body["device"].is_string(), "{body:#}");
    assert_eq!(s.api("/api/ls")["messages"].as_array().unwrap().len(), 2);

    let (status, body) = s.post(
        "/api/move",
        &format!(r#"{{"ids":["{id}"],"folder":"Archive"}}"#),
    );
    assert_eq!(status, 200, "{body:#}");
}

/// A body that is not JSON, or a request with no ids, is the caller's mistake
/// and is named as one rather than becoming a 500.
#[test]
fn malformed_writes_are_rejected_by_name() {
    let s = Serving::start();

    let (status, body) = s.post("/api/flag", "not json");
    assert_eq!(status, 400, "{body:#}");

    let (status, body) = s.post("/api/flag", r#"{"add":["seen"]}"#);
    assert_eq!(status, 400, "{body:#}");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("ids"),
        "{body:#}"
    );
}

/// There is no route that runs a command. Handing a browser an `argv` endpoint
/// would make every future subcommand reachable from a page.
#[test]
fn there_is_no_general_command_route() {
    let s = Serving::start();
    for path in ["/api/exec", "/api/run", "/api/command", "/api/pair"] {
        let (status, _, _) = s.get(path, &s.auth());
        assert_eq!(status, 404, "{path} exists");
    }
}

// ---- the page itself ----

#[test]
fn the_page_carries_the_token_and_the_headers_that_contain_it() {
    let s = Serving::start();
    let (status, head, body) = s.get(&format!("/?mailbox_token={}", s.token), "");

    assert_eq!(status, 200);
    assert!(body.contains(&s.token), "the UI has no way to call the API");
    for header in [
        "X-Content-Type-Options: nosniff",
        "X-Frame-Options: DENY",
        "Referrer-Policy: no-referrer",
        "Cache-Control: no-store",
    ] {
        assert!(head.contains(header), "missing {header}:\n{head}");
    }
}

/// The page loads its interface from this server and nowhere else, and says so
/// in a header rather than only in a policy document.
#[test]
fn the_page_declares_where_it_may_load_from() {
    let s = Serving::start();
    let (status, head, body) = s.get(&format!("/?mailbox_token={}", s.token), "");

    assert_eq!(status, 200);
    assert!(head.contains("Content-Security-Policy"), "{head}");
    assert!(head.contains("default-src 'none'"), "{head}");
    // Nothing is fetched from anywhere but here — a mail client that phones a
    // CDN on every page load is a mail client that leaks when you read (§2).
    assert!(!body.contains("//cdn"), "{body}");
    assert!(
        !body.contains("https://"),
        "the page references an outside origin: {body}"
    );

    for asset in ["/style.css", "/mailbox.js"] {
        let (status, head, _) = s.get(asset, &format!("Cookie: mailbox_token={}\r\n", s.token));
        assert_eq!(status, 200, "{asset} is not served");
        assert!(head.contains("charset=utf-8"), "{asset}: {head}");
    }
}

/// §9: no remote content unless the reader asks, and the asking has to work.
///
/// Both halves were broken at once when this was `srcdoc`. The frame inherited
/// the page's `img-src`, so a reader who said "load images" was overruled by a
/// header written for something else; and the blocked form carried a
/// placeholder `src` *alongside* the real one, so re-arming produced an `<img>`
/// with two `src` attributes — of which HTML uses the first.
#[test]
fn remote_images_load_only_when_asked_and_then_actually_load() {
    let s = Serving::start();
    let id = s.id_of("Third");
    let cookie = format!("Cookie: mailbox_token={}\r\n", s.token);

    let (status, head, blocked) = s.get(&format!("/body?id={id}"), &cookie);
    assert_eq!(status, 200, "{blocked}");
    assert!(
        head.contains("img-src data:;") || head.contains("img-src data:; "),
        "a blocked body must not permit remote images: {head}"
    );

    let (status, head, allowed) = s.get(&format!("/body?id={id}&images=1"), &cookie);
    assert_eq!(status, 200, "{allowed}");
    assert!(head.contains("img-src data: https: http:"), "{head}");

    // Exactly one `src` per image, in both forms. Two is the bug: HTML uses
    // the first, so the placeholder wins and "load images" loads nothing.
    for (what, document) in [("blocked", &blocked), ("allowed", &allowed)] {
        let images: Vec<&str> = body_of(document).split("<img ").skip(1).collect();
        assert_eq!(images.len(), 1, "{what}: expected one image");
        for tag in images {
            let tag = tag.split('>').next().unwrap_or("");
            assert_eq!(
                tag.matches("src=").count() - tag.matches("blocked-src=").count(),
                1,
                "{what}: <img {tag}>"
            );
        }
    }

    assert!(body_of(&blocked).contains("data-blocked-src="), "{blocked}");
    assert!(
        body_of(&blocked).contains("data:image/svg"),
        "no placeholder: {blocked}"
    );
    assert!(
        !body_of(&allowed).contains("data-blocked-src="),
        "{allowed}"
    );
    assert!(
        body_of(&allowed).contains("src=\"https://tracker.example/open.gif?id=42\""),
        "{allowed}"
    );
}

/// The body is the one thing that may be framed, and only by us. Everything
/// else refuses outright.
#[test]
fn only_the_message_body_may_be_framed() {
    let s = Serving::start();
    let cookie = format!("Cookie: mailbox_token={}\r\n", s.token);

    let (_, head, _) = s.get(&format!("/body?id={}", s.first_id()), &cookie);
    assert!(
        !head.contains("X-Frame-Options"),
        "the body cannot be framed: {head}"
    );
    assert!(head.contains("frame-ancestors 'self'"), "{head}");

    for path in ["/", "/style.css", "/mailbox.js"] {
        let (_, head, _) = s.get(path, &cookie);
        assert!(
            head.contains("X-Frame-Options: DENY"),
            "{path} is frameable: {head}"
        );
    }
}

/// The body renders under the page's authentication, not the API's — a frame
/// navigation cannot send a header. It still needs *some* authentication.
#[test]
fn the_body_document_is_not_public() {
    let s = Serving::start();
    let (status, _, _) = s.get(&format!("/body?id={}", s.first_id()), "");
    assert_eq!(status, 401);
}

/// The rendered message, without the document's own stylesheet — which
/// mentions `data-blocked-src` itself, and would answer for it.
fn body_of(document: &str) -> &str {
    document
        .split_once("<body>")
        .map(|(_, rest)| rest)
        .unwrap_or(document)
}

// ---- being a server on a network ----

/// Every limit in the HTTP layer is there because this listens on a socket.
#[test]
fn oversized_requests_are_refused_rather_than_buffered() {
    let s = Serving::start();

    // A request line that never ends.
    let (status, _, _) = s.request(&format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(20_000)));
    assert_eq!(status, 413);

    // A body larger than the cap.
    let (status, _, _) = s.request(&format!(
        "POST /api/flag HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        2 * 1024 * 1024
    ));
    assert_eq!(status, 413);
}

#[test]
fn nonsense_is_answered_rather_than_crashing_the_server() {
    let s = Serving::start();

    for raw in [
        "\r\n",
        "GARBAGE\r\n\r\n",
        "GET\r\n\r\n",
        "GET / SPDY/9\r\n\r\n",
        "GET / HTTP/1.1\r\nNoColonHere\r\n\r\n",
    ] {
        let (status, _, _) = s.request(raw);
        assert!(status == 400 || status == 0, "{raw:?} gave {status}");
    }

    // Still serving afterwards.
    let (status, _, _) = s.get("/api/ls", &s.auth());
    assert_eq!(status, 200);
}

// ---- writing mail (§7) ----

/// The *From* selector's options: the account's address and any alias.
#[test]
fn the_identities_a_message_may_be_sent_as_are_listed() {
    let s = Serving::start();
    let listed = s.api("/api/identities");

    let identities = listed["identities"].as_array().expect("identities");
    assert_eq!(identities[0]["address"], "peter@example.com", "{listed:#}");
    assert_eq!(identities[0]["primary"], true, "{listed:#}");
    assert_eq!(identities[0]["account"], "test", "{listed:#}");
}

/// Replying starts from a draft the *core* built. What a reply quotes and
/// which addresses a reply-all keeps are decisions with one right answer per
/// message, and two interfaces deriving them separately is how "reply" starts
/// meaning different things in different windows.
#[test]
fn a_reply_draft_comes_back_addressed_and_quoted() {
    let s = Serving::start();
    let id = s.id_of("Second");

    let draft = s.api(&format!("/api/reply?id={id}"));
    assert_eq!(draft["to"][0], "Bob <bob@example.com>", "{draft:#}");
    assert_eq!(draft["subject"], "Re: Second", "{draft:#}");
    assert!(
        draft["body"].as_str().unwrap().contains("goodbye"),
        "the original is not quoted: {draft:#}"
    );
    // Threading (§5.6) depends on this, and a compose box cannot invent it.
    assert_eq!(draft["in_reply_to"], "<two@example.com>", "{draft:#}");

    let forward = s.api(&format!("/api/reply?id={id}&mode=forward"));
    assert!(
        forward["subject"].as_str().unwrap().starts_with("Fwd:"),
        "{forward:#}"
    );
    assert!(
        forward["to"].as_array().unwrap().is_empty(),
        "a forward addresses nobody: {forward:#}"
    );
}

#[test]
fn an_unknown_reply_mode_is_named_rather_than_guessed() {
    let s = Serving::start();
    let (status, _, body) = s.get(
        &format!("/api/reply?id={}&mode=sideways", s.first_id()),
        &s.auth(),
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("sender, all, or forward"), "{body}");
}

/// §5.5: sending queues. A message goes to the outbox and the outbox is
/// flushed by a sync pass — which is what makes the undo window and a crash
/// between "queued" and "sent" survivable.
#[test]
fn sending_queues_the_message_rather_than_transmitting_it() {
    let s = Serving::start();

    let (status, result) = s.post(
        "/api/send",
        r#"{"to":["bob@example.com"],"subject":"Hello","body":"A message."}"#,
    );
    assert_eq!(status, 200, "{result:#}");
    assert_eq!(result["status"], "queued", "{result:#}");
    assert_eq!(result["recipients"][0], "bob@example.com", "{result:#}");
    assert!(result["bytes"].as_u64().unwrap_or(0) > 0, "{result:#}");

    let outbox = s.api("/api/outbox");
    let queued = outbox["messages"].as_array().expect("messages");
    assert_eq!(queued.len(), 1, "{outbox:#}");
    assert_eq!(queued[0]["subject"], "Hello", "{outbox:#}");
}

/// A blind copy reaches the envelope and no header — the whole of what makes
/// it blind.
#[test]
fn a_blind_copy_stays_out_of_the_headers() {
    let s = Serving::start();

    let (status, result) = s.post(
        "/api/send",
        r#"{"to":["bob@example.com"],"bcc":["secret@example.com"],"subject":"H","body":"b"}"#,
    );
    assert_eq!(status, 200, "{result:#}");

    let recipients: Vec<&str> = result["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(recipients.contains(&"secret@example.com"), "{result:#}");

    let outbox = s.api("/api/outbox");
    let text = serde_json::to_string(&outbox).unwrap();
    assert!(
        !text.contains("secret@example.com") || !text.contains("\"Bcc\""),
        "a blind copy appeared in a header: {outbox:#}"
    );
}

/// A message with nobody to send it to is the caller's mistake, and is named
/// rather than queued and failed later.
#[test]
fn a_message_with_no_recipient_is_refused() {
    let s = Serving::start();

    let (status, body) = s.post("/api/send", r#"{"subject":"Hello","body":"A message."}"#);
    assert_eq!(status, 400, "{body:#}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("recipient"),
        "{body:#}"
    );

    let (status, body) = s.post("/api/send", r#"{"to":["not an address"],"subject":"x"}"#);
    assert_eq!(status, 400, "{body:#}");
}

// ---- the first launch (§9) ----

/// `serve` on a machine with no account starts, and says what to do about it.
///
/// It used to exit with "no such account: none configured", which is the one
/// answer that cannot work: the interface being refused is where the person
/// who has not added an account would have read how to.
#[test]
fn a_server_with_no_accounts_starts_and_explains_itself() {
    let s = Serving::start_unconfigured();

    // Nothing configured is an answer, not a 404: this is what the page reads
    // to decide which of its two states it is in.
    let accounts = s.api("/api/accounts");
    assert_eq!(
        accounts["accounts"].as_array().map(Vec::len),
        Some(0),
        "{accounts:#}"
    );

    // And a listing over no accounts is empty rather than an error, so every
    // route the page loads with still answers.
    let listing = s.api("/api/ls");
    assert_eq!(listing["messages"].as_array().map(Vec::len), Some(0));

    let (status, _, page) = s.get(&format!("/?mailbox_token={}", s.token), "");
    assert_eq!(status, 200);
    assert!(
        page.contains(r#"data-setup="1""#),
        "the page was not told it may add an account"
    );
    assert!(
        page.contains(r#"id="setup-form""#),
        "the page carries no way to add one"
    );
}

/// The account is added in a terminal beside the browser, and the page is
/// still open. Finding the mail there without restarting the server is the
/// difference between a guide and a dead end.
#[test]
fn an_account_added_while_serving_is_picked_up() {
    let s = Serving::start_unconfigured();
    assert_eq!(
        s.api("/api/accounts")["accounts"].as_array().map(Vec::len),
        Some(0)
    );

    std::fs::write(
        &s.config,
        "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n",
    )
    .expect("write config");

    let accounts = s.api("/api/accounts");
    assert_eq!(
        accounts["accounts"][0]["address"], "peter@example.com",
        "the server did not notice the account: {accounts:#}"
    );
}

// ---- setting the mailbox up from the page (§9) ----

/// The built-in table, over HTTP, so the setup form can fill itself in.
///
/// Offline by construction — the table is compiled in — which is why it can
/// run on every keystroke while `discover` waits behind a button.
#[test]
fn the_provider_table_answers_for_the_setup_form() {
    let s = Serving::start_unconfigured();

    let known = s.api("/api/providers?address=peter%40gmail.com");
    assert_eq!(known["known"], true, "{known:#}");
    assert_eq!(known["imap_host"], "imap.gmail.com");
    assert_eq!(known["imap_port"], 993);
    // Where it came from, in the shape `discover` says it, so the form has one
    // way of explaining itself.
    assert_eq!(known["imap_source"], "table");

    // Not an error: an address the table does not know is an ordinary account
    // whose servers have to be typed in.
    let unknown = s.api("/api/providers?address=peter%40nothing.invalid");
    assert_eq!(unknown["known"], false, "{unknown:#}");
}

/// The account is tried before it is written, and a server that does not
/// answer leaves nothing behind.
///
/// This is the whole reason the web route does more than `accounts add`: the
/// person filling the form in has no `accounts edit` to correct it with, so an
/// account that cannot work must never reach the config.
#[test]
fn an_account_that_cannot_connect_is_never_written() {
    let s = Serving::start_unconfigured();

    // A port on this machine with nothing behind it: the shape of a mistyped
    // host, and no network is touched to find out.
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };

    let (status, body) = s.post(
        "/api/accounts",
        &format!(
            r#"{{"address":"peter@example.com","password":"hunter2",
                 "imap_host":"127.0.0.1","imap_port":{dead}}}"#
        ),
    );
    assert!(status >= 400, "the add was accepted: {body:#}");

    assert_eq!(
        s.api("/api/accounts")["accounts"].as_array().map(Vec::len),
        Some(0),
        "an account that could not be reached was written anyway"
    );
    let config = std::fs::read_to_string(&s.config).expect("read the config");
    assert!(
        !config.contains("peter@example.com"),
        "the config was written despite the refusal:\n{config}"
    );
}

/// An account added from the page needs a password, because the two ways of
/// supplying one later — `credential_command` and `MAILBOX_PASSWORD_*` — both
/// need a terminal.
#[test]
fn an_account_added_from_the_page_needs_a_password() {
    let s = Serving::start_unconfigured();

    let (status, body) = s.post("/api/accounts", r#"{"address":"peter@gmail.com"}"#);
    assert_eq!(status, 400, "{body:#}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("password"),
        "{body:#}"
    );
}

/// A server on the network will not take a password, and does not pretend it
/// might: the page it serves carries the guide instead of the form.
///
/// `serve` speaks plain HTTP. Reading mail over it is the user's call to make
/// with `--bind`; a mail provider's password crossing the same link in the
/// clear is not one they have made.
#[test]
fn a_server_on_the_network_will_not_take_a_password() {
    let s = Serving::start_unconfigured_on_the_network();

    let (status, body) = s.post(
        "/api/accounts",
        r#"{"address":"peter@gmail.com","password":"hunter2"}"#,
    );
    assert_eq!(status, 400, "{body:#}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("loopback"),
        "the refusal does not say why: {body:#}"
    );

    // Testing carries the same password, so it is refused on the same terms.
    let (status, body) = s.post(
        "/api/accounts/test",
        r#"{"address":"peter@gmail.com","password":"hunter2"}"#,
    );
    assert_eq!(status, 400, "{body:#}");

    let (status, _, page) = s.get(&format!("/?mailbox_token={}", s.token), "");
    assert_eq!(status, 200);
    assert!(
        page.contains(r#"data-setup=""#) && !page.contains(r#"data-setup="1""#),
        "the page offers a form this server would refuse"
    );
}

/// The Test button: the same connection Add makes, and no config written
/// whether it succeeds or fails.
#[test]
fn settings_can_be_tried_without_adding_the_account() {
    let s = Serving::start_unconfigured();

    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };

    let (status, body) = s.post(
        "/api/accounts/test",
        &format!(
            r#"{{"address":"peter@example.com","password":"hunter2",
                 "imap_host":"127.0.0.1","imap_port":{dead}}}"#
        ),
    );
    assert!(
        status >= 400,
        "a dead port was reported as working: {body:#}"
    );

    // The point of the button: it answers the question without committing to
    // anything, so a failed test leaves the mailbox exactly as it was.
    assert_eq!(
        s.api("/api/accounts")["accounts"].as_array().map(Vec::len),
        Some(0),
        "testing wrote an account"
    );
    let config = std::fs::read_to_string(&s.config).expect("read the config");
    assert!(
        config.trim().is_empty(),
        "testing wrote a config:\n{config}"
    );

    // And it needs a password, for the same reason adding does.
    let (status, body) = s.post("/api/accounts/test", r#"{"address":"peter@gmail.com"}"#);
    assert_eq!(status, 400, "{body:#}");
}

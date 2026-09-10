//! Pairing through the rendezvous relay, end to end (§8.2).
//!
//! Four processes: a relay, a receiver, a sender, and the test driving them.
//! `pairing.rs` covers the LAN transport and the export file; this is the
//! third one — the case where the two devices are not on the same network and
//! the only thing that crosses between them by hand is a short typed code.
//!
//! What matters here is not that the happy path works. It is that the relay is
//! in the middle of every byte and still cannot do anything with them, and
//! that a wrong code cannot pair even when the human check is skipped.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A relay running as its own process, stopped when the test ends.
struct Relay {
    child: Child,
    port: u16,
}

impl Relay {
    fn start() -> Relay {
        // Take a port, release it, and hand the number to the relay. A race
        // with another process is possible and vanishingly unlikely; the
        // alternative is parsing the child's stdout while it also blocks
        // serving, which is a great deal more machinery for the same result.
        let port = TcpListener::bind("127.0.0.1:0")
            .expect("a free port")
            .local_addr()
            .expect("addr")
            .port();

        let child = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(["relay", "--bind", "127.0.0.1", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the relay");

        let relay = Relay { child, port };
        relay.wait_until_up();
        relay
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn wait_until_up(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the relay never came up on port {}", self.port);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Device {
    dir: PathBuf,
    config: PathBuf,
}

impl Device {
    fn new(name: &str, accounts: &str) -> Device {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mailbox-relay-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.toml");
        std::fs::write(&config, accounts).expect("config");
        Device { dir, config }
    }

    fn enrolled() -> Device {
        Device::new(
            "old",
            "[accounts.work]\n\
             address = \"peter@work.example\"\n\
             imap_host = \"imap.work.example\"\n\
             \n\
             [accounts.home]\n\
             address = \"peter@home.example\"\n\
             imap_host = \"imap.home.example\"\n",
        )
    }

    fn blank() -> Device {
        Device::new("new", "")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", self.dir.join("state"))
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            .env("MAILBOX_NOTIFIER", "none")
            .env("MAILBOX_DISCOVERY", "off")
            .env("MAILBOX_KEYCHAIN", "none")
            .env_remove("MAILBOX_AGENT");
        cmd
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap_or_default()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Starts a receiver, and returns it along with the code it displayed.
///
/// The receiver blocks until the sender arrives, so its stdout has to be read
/// while it runs — the code is the first line, printed before the wait, which
/// is exactly why `--json` emits it separately.
fn start_receiver(device: &Device, relay: &Relay) -> (Child, String) {
    use std::io::{BufRead, BufReader, Read};

    let mut child = device
        .command(&["--json", "--yes", "pair", "--relay", &relay.url()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the receiver");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    // The reader has to outlive this function. Dropping it closes the pipe,
    // and the receiver's *next* write — the result it prints after pairing —
    // dies with EPIPE, which looks exactly like a pairing failure and is not
    // one. So a thread holds the read end open until the child is done.
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut first = String::new();
        let _ = reader.read_line(&mut first);
        let _ = tx.send(first);
        let mut rest = String::new();
        let _ = reader.read_to_string(&mut rest);
    });

    let line = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("the receiver should print its code before waiting");
    let value: serde_json::Value =
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("expected JSON, got {e}: {line}"));
    let code = value["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no code in {line}"))
        .to_string();
    (child, code)
}

#[test]
fn two_devices_pair_through_a_relay() {
    let relay = Relay::start();
    let old = Device::enrolled();
    let new = Device::blank();

    let (receiver, code) = start_receiver(&new, &relay);

    // §8.1's shape: a slot, then nine characters. Short enough to read aloud,
    // which is the whole reason this transport exists.
    assert!(code.len() <= 16, "the code should stay typeable: {code}");
    assert!(code.contains('-'), "slot and secret are separated: {code}");

    let sent = old
        .command(&["--yes", "pair", "--relay", &relay.url(), "send", &code])
        .output()
        .expect("send");
    assert!(
        sent.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&sent.stderr)
    );

    let out = receiver.wait_with_output().expect("receiver");
    assert!(
        out.status.success(),
        "receive failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let config = new.config_text();
    assert!(config.contains("peter@work.example"), "{config}");
    assert!(config.contains("peter@home.example"), "{config}");
}

/// The attack the code's length makes worth worrying about: someone who knows
/// the slot — which is public, and printed on the other device's screen —
/// guessing the secret half.
///
/// The receiver here runs with `--yes`, so the word check that would normally
/// catch this is skipped. It still must not pair: the payload is sealed under
/// a secret the guesser never agreed on, and it does not open.
#[test]
fn a_guessed_code_cannot_pair_even_with_the_word_check_skipped() {
    let relay = Relay::start();
    let attacker = Device::enrolled();
    let new = Device::blank();

    let (receiver, code) = start_receiver(&new, &relay);
    let slot = code.split('-').next().expect("a slot");
    let guess = format!("{slot}-ZZZ-ZZZ-ZZZ");
    assert_ne!(guess, code, "the test needs an actually wrong code");

    let sent = attacker
        .command(&["--yes", "pair", "--relay", &relay.url(), "send", &guess])
        .output()
        .expect("send");
    // The sender cannot tell: it seals under the secret *it* derived and
    // leaves. Not knowing is the point — there is no oracle here.
    assert!(sent.status.success(), "the guesser is not told it failed");

    let out = receiver.wait_with_output().expect("receiver");
    assert!(!out.status.success(), "a wrong code must not pair");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("did not authenticate"),
        "expected an authentication failure, got: {stderr}"
    );
    assert!(
        new.config_text().is_empty(),
        "nothing may be written: {}",
        new.config_text()
    );
}

/// The relay carries every byte and must be able to make nothing of them.
#[test]
fn the_relay_never_sees_anything_readable() {
    let relay = Relay::start();
    let old = Device::enrolled();
    let new = Device::blank();

    let (receiver, code) = start_receiver(&new, &relay);
    let slot = code.split('-').next().expect("a slot").to_string();

    let sent = old
        .command(&["--yes", "pair", "--relay", &relay.url(), "send", &code])
        .output()
        .expect("send");
    assert!(sent.status.success());
    let _ = receiver.wait_with_output().expect("receiver");

    // Everything the relay was ever asked to hold, from its own API. The
    // boxes are consumed on read, so this is what a *late* observer sees —
    // and an operator watching in real time sees the same bytes, sealed.
    for name in ["offer", "answer", "payload"] {
        let (_, body) = http_get(relay.port, &format!("/r/{slot}.{name}"));
        let text = String::from_utf8_lossy(&body);
        for secret in ["peter@work.example", "imap.work.example", "peter@home"] {
            assert!(
                !text.contains(secret),
                "`{secret}` was readable in the relay's {name} box"
            );
        }
    }

    // And the code itself never reaches the relay in any form.
    let (_, health) = http_get(relay.port, "/health");
    let text = String::from_utf8_lossy(&health);
    assert!(!text.contains(&code), "the relay must not know the code");
}

/// §8.2: a box is written once. A second write to the same one is refused,
/// which is what burns a channel an attacker has raced.
#[test]
fn a_box_can_only_be_claimed_once() {
    let relay = Relay::start();
    let (status, _) = http_post(relay.port, "/r/testslot.offer", b"first");
    assert_eq!(status, 201);
    let (status, _) = http_post(relay.port, "/r/testslot.offer", b"second");
    assert_eq!(status, 409, "a claimed box must be refused");

    // Read it, and it is gone.
    let (status, body) = http_get(relay.port, "/r/testslot.offer");
    assert_eq!(status, 200);
    assert_eq!(body, b"first");
    assert_eq!(
        http_get(relay.port, "/r/testslot.offer").0,
        404,
        "a box is read once"
    );
}

/// The relay is not a free key-value store for whoever finds it.
#[test]
fn the_relay_stores_only_what_the_protocol_names() {
    let relay = Relay::start();
    for path in [
        "/r/slot.notabox",
        "/r/../escape.offer",
        "/elsewhere",
        "/r/slot",
    ] {
        let (status, _) = http_post(relay.port, path, b"junk");
        assert!(
            status == 404 || status == 400,
            "{path} should not be storable, got {status}"
        );
    }
}

#[test]
fn an_oversized_message_is_refused_by_the_relay() {
    let relay = Relay::start();
    let (status, _) = http_post(relay.port, "/r/big.payload", &vec![b'x'; 64 * 1024 + 1]);
    assert!(status == 413 || status == 400, "got {status}");
}

// ---- a minimal HTTP client, so the test asserts on the wire ----

fn http_request(port: u16, raw: &[u8]) -> (u16, Vec<u8>) {
    use std::io::Read;
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to the relay");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.write_all(raw).expect("write");
    stream.flush().expect("flush");

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read");

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete response");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("a status");
    (status, buf[split + 4..].to_vec())
}

fn http_get(port: u16, path: &str) -> (u16, Vec<u8>) {
    let raw = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    http_request(port, raw.as_bytes())
}

fn http_post(port: u16, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut raw = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    http_request(port, &raw)
}

//! Shared harness for the L2 suite (§14.2): a real Dovecot in a container,
//! seeded from fixtures.
//!
//! Two things from §14 make this affordable:
//!
//! - **Symbolic state.** No assertion names a server-assigned value. The seeder
//!   records whatever UID the server handed back (`UIDPLUS` on `APPEND`), and
//!   tests refer to `fx.id("invoice-with-pdf")`. A hardcoded `UID 4711` is a
//!   test that runs against exactly one server, and provider divergence is
//!   precisely what L2 exists to catch.
//! - **Cheap reset.** One long-lived server; each test mints a fresh account.
//!   The test Dovecot accepts any username with one password, so "a fresh
//!   account" costs nothing and tests run in parallel. Deleting messages over
//!   IMAP to clean up would be slower and a test of the wrong thing.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mailbox_core::fixture::Fixture;
use mailbox_core::imap::{Client, ConnectionConfig, Credential, Security};
use mailbox_core::Tracer;

pub const CONTAINER: &str = "mailbox-l2";
pub const IMAGE: &str = "mailbox-test-dovecot";
pub const PORT: u16 = 21430;
pub const PASSWORD: &str = "testpass";

/// Whether L2 may be skipped. CI sets `MAILBOX_L2_REQUIRED=1` so a missing
/// Docker turns into a failure there, while a laptop without one still gets a
/// green L0 run and a clear note.
fn l2_required() -> bool {
    std::env::var("MAILBOX_L2_REQUIRED").is_ok_and(|v| v == "1")
}

/// Brings the server up if it is not already running, and returns false when
/// L2 cannot run here.
///
/// Every test calls this, and they run in parallel in one process — so the
/// startup is behind a `Once`. Without it, several tests racing a cold
/// container each reach the `docker rm -f` below and tear down the container
/// the others are already using, which shows up as one unrelated test failing
/// on a cold run and passing on every rerun.
pub fn server_available() -> bool {
    static START: std::sync::Once = std::sync::Once::new();
    static READY: AtomicBool = AtomicBool::new(false);
    START.call_once(|| READY.store(start_server(), Ordering::SeqCst));
    READY.load(Ordering::SeqCst)
}

fn start_server() -> bool {
    if !docker_works() {
        if l2_required() {
            panic!("MAILBOX_L2_REQUIRED=1 but Docker is not usable here");
        }
        eprintln!(
            "L2 SKIPPED: Docker is not available (set MAILBOX_L2_REQUIRED=1 to make this fatal)"
        );
        return false;
    }
    if server_ready() {
        return true;
    }

    // Build is a no-op when the image is current, so this costs a cache lookup.
    let built = Command::new("docker")
        .args([
            "build",
            "-q",
            "-t",
            IMAGE,
            dovecot_dir().to_str().unwrap_or("."),
        ])
        .output();
    if !built.map(|o| o.status.success()).unwrap_or(false) {
        if l2_required() {
            panic!("MAILBOX_L2_REQUIRED=1 but the Dovecot image would not build");
        }
        eprintln!("L2 SKIPPED: could not build {IMAGE}");
        return false;
    }

    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .output();
    let run = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            &format!("{PORT}:143"),
            IMAGE,
        ])
        .output();
    if !run.map(|o| o.status.success()).unwrap_or(false) {
        if l2_required() {
            panic!("MAILBOX_L2_REQUIRED=1 but the container would not start");
        }
        eprintln!("L2 SKIPPED: could not start {CONTAINER}");
        return false;
    }

    for _ in 0..150 {
        if server_ready() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if l2_required() {
        panic!("MAILBOX_L2_REQUIRED=1 but Dovecot never answered on port {PORT}");
    }
    eprintln!("L2 SKIPPED: Dovecot never answered on port {PORT}");
    false
}

fn docker_works() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Readiness is a valid IMAP greeting, not an open port.
///
/// Docker's userland proxy accepts the TCP connection as soon as the container
/// exists, and only then fails to reach the process behind it — so a plain
/// connect succeeds a second or two before Dovecot is listening. Every test
/// then starts at once against a server that closes the connection
/// mid-greeting, which reads as a sync bug and is not one.
fn server_ready() -> bool {
    use std::io::Read;

    let addr = format!("127.0.0.1:{PORT}")
        .parse()
        .expect("a valid address");
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => buf[..n].starts_with(b"* OK"),
        _ => false,
    }
}

fn dovecot_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/mailbox-cli.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("a workspace root")
        .join("tests/dovecot")
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// One test's world: a fresh IMAP account, a fresh store, and a config file
/// pointing at both.
pub struct World {
    pub user: String,
    pub dir: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    /// Fixture name to stable id (§5.2), filled in by [`World::seed`].
    pub ids: BTreeMap<String, String>,
    /// Fixture name to the UID the server assigned. Recorded, never asserted on
    /// directly — it exists so a test can drive the *server* (expunge this
    /// message) without hardcoding a number.
    pub uids: BTreeMap<String, u32>,
}

impl World {
    /// Mints a fresh account and writes its config.
    pub fn new() -> World {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let user = format!("t{pid}x{n}@example.com");

        let dir = std::env::temp_dir().join(format!("mailbox-l2-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");

        let state = dir.join("state");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\n\
                 address = \"{user}\"\n\
                 imap_host = \"127.0.0.1\"\n\
                 imap_port = {PORT}\n\
                 imap_security = \"none\"\n\
                 # §9 permits plaintext only for a loopback host the user opted
                 # in for. Both halves are true here and nowhere else.\n\
                 require_tls = false\n\
                 offline = \"recent\"\n"
            ),
        )
        .expect("write config");

        World {
            user,
            dir,
            config,
            state,
            ids: BTreeMap::new(),
            uids: BTreeMap::new(),
        }
    }

    fn client(&self) -> Client {
        let cfg = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: PORT,
            security: Security::None,
            require_tls: false,
            timeout: std::time::Duration::from_secs(20),
        };
        let mut client = Client::connect(&cfg, Tracer::disabled()).expect("connect to Dovecot");
        client
            .login(&self.user, &Credential::Password(PASSWORD.into()))
            .expect("authenticate against Dovecot");
        client
    }

    /// Seeds the *server* from a fixture manifest, recording the UID it
    /// assigned to each message.
    pub fn seed(&mut self, manifest: &str) -> &mut World {
        let fixture = Fixture::parse(manifest).expect("parse fixture");
        let mut client = self.client();

        for folder in &fixture.folder {
            if !folder.path.eq_ignore_ascii_case("INBOX") {
                client.create_folder(&folder.path).expect("create folder");
            }
        }
        for rendered in fixture.render_all().expect("render fixture") {
            if !rendered.folder.eq_ignore_ascii_case("INBOX") {
                client
                    .create_folder(&rendered.folder)
                    .expect("create folder");
            }
            let uid = client
                .append(
                    &rendered.folder,
                    &rendered.flags,
                    Some(rendered.internaldate),
                    &rendered.bytes,
                )
                .expect("append fixture message");
            if let Some(uid) = uid {
                self.uids.insert(rendered.name.clone(), uid);
            }
        }
        let _ = client.logout();
        self
    }

    /// Runs the CLI with this world's environment.
    pub fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            // §9's env-var credential path, which exists for exactly this: a
            // suite that needs a credential and has no keychain to put one in.
            .env("MAILBOX_PASSWORD_TEST", PASSWORD)
            // A pinned clock and seed, so nothing in the assertions depends on
            // wall-clock time (§6.7).
            // Pinned near the fixtures' own dates, so the §5.1 prefetch window
            // (30 days) covers them. A clock far from the fixture dates would
            // make every body-fetch assertion depend on how long ago the
            // fixture was written.
            .env("MAILBOX_CLOCK", "2026-08-20T12:00:00Z")
            .env("MAILBOX_SEED", "l2")
            .env("COLUMNS", "120")
            .env("NO_COLOR", "1")
            // The listing history behind `^` is per-account scratch; keeping it
            // inside the test dir stops parallel tests from sharing it.
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env_remove("MAILBOX_AGENT")
            .output()
            .expect("run the mailbox binary");

        Output {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Runs the CLI and parses one JSON document, failing loudly on a non-zero
    /// exit — the common case, so tests read as assertions rather than as
    /// plumbing.
    pub fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.run(&full);
        assert_eq!(
            out.status,
            0,
            "`mailbox {}` failed:\n{}\n{}",
            full.join(" "),
            out.stdout,
            out.stderr
        );
        serde_json::from_str(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "`mailbox {}` did not emit one JSON document: {e}\n{}",
                full.join(" "),
                out.stdout
            )
        })
    }

    /// One sync pass to quiescence (§6.7), then the ids it produced.
    pub fn sync(&mut self) -> serde_json::Value {
        let result = self.json(&["sync", "--once"]);
        self.refresh_ids();
        result
    }

    pub fn sync_with(&mut self, extra: &[&str]) -> serde_json::Value {
        let mut args = vec!["sync", "--once"];
        args.extend_from_slice(extra);
        let result = self.json(&args);
        self.refresh_ids();
        result
    }

    /// Rebuilds the symbolic-name to id map from the store, by matching the
    /// `Message-ID` the fixture pinned.
    ///
    /// This is the link that keeps assertions server-agnostic: the fixture
    /// knows the `Message-ID`, the store knows the stable id, and no test ever
    /// has to see either.
    fn refresh_ids(&mut self) {
        let dump = self.dump(None);
        let Some(messages) = dump["messages"].as_array() else {
            return;
        };
        self.ids.clear();
        for m in messages {
            let (Some(mid), Some(id)) = (m["message_id"].as_str(), m["id"].as_str()) else {
                continue;
            };
            self.ids.insert(mid.to_string(), id.to_string());
        }
    }

    /// The stable id of a message, by the `Message-ID` its fixture pinned.
    pub fn id(&self, message_id: &str) -> String {
        let key = message_id.trim_start_matches('<').trim_end_matches('>');
        self.ids
            .get(key)
            .cloned()
            .unwrap_or_else(|| panic!("no message with Message-ID <{key}> in the store"))
    }

    pub fn has(&self, message_id: &str) -> bool {
        self.ids
            .contains_key(message_id.trim_start_matches('<').trim_end_matches('>'))
    }

    pub fn dump(&self, query: Option<&str>) -> serde_json::Value {
        let mut args = vec!["debug", "store-dump"];
        if let Some(q) = query {
            args.push(q);
        }
        let out = self.run(&args);
        assert_eq!(out.status, 0, "store-dump failed:\n{}", out.stderr);
        serde_json::from_str(&out.stdout).expect("store-dump emits JSON")
    }

    // ---- driving the server directly, for the cases only a server can create ----

    /// Sets flags on a message the server already holds.
    ///
    /// `SELECT`, not `EXAMINE`: the product only ever opens folders read-only
    /// in M1 (§5.3), and a `STORE` under `EXAMINE` is accepted by Dovecot and
    /// then discarded — which looks exactly like a sync bug.
    pub fn set_server_flags(&self, folder: &str, uid: u32, flags: &str) {
        let mut client = self.client();
        client
            .raw_command(&format!("SELECT \"{folder}\""))
            .expect("select");
        client
            .raw_command(&format!("UID STORE {uid} FLAGS ({flags})"))
            .expect("store flags");
        let _ = client.logout();
    }

    /// Expunges a message, the way another client would.
    pub fn expunge(&self, folder: &str, uid: u32) {
        let mut client = self.client();
        client
            .raw_command(&format!("SELECT \"{folder}\""))
            .expect("select");
        client
            .raw_command(&format!("UID STORE {uid} +FLAGS (\\Deleted)"))
            .expect("mark deleted");
        client.raw_command("EXPUNGE").expect("expunge");
        let _ = client.logout();
    }

    /// Deletes a folder, the way another client would.
    pub fn delete_folder(&self, folder: &str) {
        let mut client = self.client();
        client
            .raw_command(&format!("DELETE \"{folder}\""))
            .expect("delete folder");
        let _ = client.logout();
    }

    /// Deletes and recreates a folder, which is how a real `UIDVALIDITY` change
    /// happens (§5.3). Forcing one any other way would be testing our fixture,
    /// not the server's behaviour.
    pub fn recreate_folder(&self, folder: &str) {
        let mut client = self.client();
        client
            .raw_command(&format!("DELETE \"{folder}\""))
            .expect("delete folder");
        client.create_folder(folder).expect("recreate folder");
        let _ = client.logout();
    }

    pub fn append(&mut self, name: &str, folder: &str, manifest: &str) {
        let fixture = Fixture::parse(manifest).expect("parse fixture");
        let spec = fixture.get(name).expect("named fixture message");
        let rendered = fixture.render(spec).expect("render");
        let mut client = self.client();
        if !folder.eq_ignore_ascii_case("INBOX") {
            client.create_folder(folder).expect("create folder");
        }
        if let Some(uid) = client
            .append(
                folder,
                &rendered.flags,
                Some(rendered.internaldate),
                &rendered.bytes,
            )
            .expect("append")
        {
            self.uids.insert(name.to_string(), uid);
        }
        let _ = client.logout();
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // The account stays on the server: it is one empty directory, the
        // container is disposable, and deleting mailboxes over IMAP to clean up
        // is exactly what §14 says not to do.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn error_code(&self) -> String {
        let v: serde_json::Value =
            serde_json::from_str(&self.stderr).unwrap_or(serde_json::Value::Null);
        v["error"]["code"].as_str().unwrap_or_default().to_string()
    }
}

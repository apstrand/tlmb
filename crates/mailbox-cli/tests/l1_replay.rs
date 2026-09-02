//! **L1 — recorded-transcript replay** (§14.2).
//!
//! > Recorded-transcript replay against the sync engine (VCR-style). ms. Gates
//! > PRs.
//!
//! The load-bearing assertion is not "the replay ran" — it is that **a replayed
//! conversation produces the same store as the live one did**. Anything weaker
//! would let the recording drift away from what the server actually said while
//! the suite kept passing.
//!
//! Transcripts here are recorded from the L3 IMAP server rather than from
//! Dovecot, because Docker is not reachable on this machine. §14.2 wants them
//! recorded from L2 and L4; the mechanism is identical either way —
//! `MAILBOX_RECORD` arms the real client on the real sync path — and
//! `tests/transcripts/` should be re-recorded against real servers when one is
//! available.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use mailbox_l3::{ImapServer, Message};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
    /// Held for the world's lifetime so the port stays alive while recording;
    /// nothing reads it, and dropping it stops the server.
    _server: Option<ImapServer>,
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

impl World {
    /// A world with a live server to record from.
    fn live() -> World {
        let server = ImapServer::start(vec![
            Message::simple(1, "First", "one@example.com"),
            Message::simple(2, "Second", "two@example.com"),
            Message::simple(3, "Third", "three@example.com"),
        ])
        .expect("start the L3 IMAP server");
        World::build(Some(server))
    }

    /// A world with no server at all — the state a replay runs in.
    fn offline() -> World {
        World::build(None)
    }

    fn build(server: Option<ImapServer>) -> World {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-l1-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        // Port 1 when there is no server: a replay that secretly opened a
        // socket would fail loudly rather than pass quietly.
        let port = server.as_ref().map(|s| s.port()).unwrap_or(1);
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\n\
                 address = \"peter@example.com\"\n\
                 imap_host = \"127.0.0.1\"\n\
                 imap_port = {port}\n\
                 imap_security = \"none\"\n\
                 require_tls = false\n"
            ),
        )
        .expect("write config");

        World {
            state: dir.join("state"),
            config,
            dir,
            _server: server,
        }
    }

    fn run_with(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            .env("MAILBOX_CLOCK", "2026-09-02T12:00:00Z")
            .env("MAILBOX_SEED", "l1")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            .env_remove("MAILBOX_RECORD")
            .env_remove("MAILBOX_RECORD_PATH");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run the binary");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        self.run_with(args, &[])
    }

    /// Syncs while recording the conversation to `path`.
    fn record(&self, path: &std::path::Path, scenario: &str) -> Run {
        self.run_with(
            &["--json", "sync", "--once"],
            &[
                ("MAILBOX_RECORD", scenario),
                ("MAILBOX_RECORD_PATH", path.to_str().unwrap()),
            ],
        )
    }

    /// The canonical dump, with the fields a replay cannot reproduce removed.
    fn dump(&self) -> serde_json::Value {
        let run = self.run(&["debug", "store-dump"]);
        assert_eq!(run.status, 0, "store-dump failed:\n{}", run.stderr);
        serde_json::from_str(&run.stdout).expect("dump")
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Recording captures a whole conversation, and the header says what it is a
/// recording of — §14.2 requires the date and the server, because a stale
/// transcript is a test about a server that no longer exists.
#[test]
fn recording_captures_the_conversation_and_says_what_it_recorded() {
    let w = World::live();
    let path = w.dir.join("basic.ndjson");

    let run = w.record(&path, "a first sync of three messages");
    assert_eq!(run.status, 0, "{}", run.stderr);

    let text = std::fs::read_to_string(&path).expect("a transcript");
    let header: serde_json::Value =
        serde_json::from_str(text.lines().next().expect("a header")).expect("header JSON");

    assert_eq!(header["scenario"], "a first sync of three messages");
    assert!(
        header["recorded_at"]
            .as_str()
            .unwrap()
            .starts_with("2026-09-02"),
        "the recording is stamped with the injected clock: {header}"
    );
    assert!(
        !header["server"].as_str().unwrap_or_default().is_empty(),
        "the recording should say what server it came from: {header}"
    );

    // A real conversation, not a connection error.
    let sent = text.lines().filter(|l| l.contains("\"sent\"")).count();
    assert!(sent > 5, "only {sent} commands were recorded");
}

/// The property that makes L1 worth gating on: replaying the recording builds
/// the same store the live sync did.
#[test]
fn a_replayed_transcript_reproduces_the_live_store() {
    let live = World::live();
    let path = live.dir.join("sync.ndjson");
    assert_eq!(live.record(&path, "first sync").status, 0);
    let expected = live.dump();
    assert_eq!(expected["messages"].as_array().unwrap().len(), 3);

    // A different world, with no server reachable at all.
    let offline = World::offline();
    let run = offline.run(&["--json", "debug", "replay", path.to_str().unwrap()]);
    assert_eq!(
        run.status, 0,
        "replay failed:\n{}\n{}",
        run.stdout, run.stderr
    );

    let replayed = offline.dump();
    assert_eq!(
        replayed["messages"], expected["messages"],
        "the replayed store differs from the live one"
    );
    assert_eq!(replayed["threads"], expected["threads"]);
    assert_eq!(replayed["placements"], expected["placements"]);
    assert_eq!(replayed["folders"], expected["folders"]);
}

/// A replay must not touch the network. The config points at port 1, so a
/// replay that opened a socket would fail — and this asserts it does not.
#[test]
fn a_replay_needs_no_server() {
    let live = World::live();
    let path = live.dir.join("sync.ndjson");
    assert_eq!(live.record(&path, "first sync").status, 0);

    // Prove the port really is dead for this world before trusting the replay.
    let offline = World::offline();
    let sync = offline.run(&["--json", "sync", "--once"]);
    assert_ne!(sync.status, 0, "the port should be unreachable");

    let replay = offline.run(&["--json", "debug", "replay", path.to_str().unwrap()]);
    assert_eq!(replay.status, 0, "{}", replay.stderr);
    let payload: serde_json::Value = serde_json::from_str(&replay.stdout).expect("JSON");
    assert_eq!(payload["synced"][0]["messages_added"], 3);
    assert!(payload["diverged"].is_null());
}

/// Replay asserts on what the client *sent*, not just on what it read. A
/// recording that only replayed responses would let the engine send anything at
/// all and still pass.
#[test]
fn a_changed_conversation_is_caught() {
    let live = World::live();
    let path = live.dir.join("sync.ndjson");
    assert_eq!(live.record(&path, "first sync").status, 0);

    // Doctor the recording so it expects a command the engine will not send.
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let victim = lines
        .iter()
        .position(|l| l.contains("\"sent\""))
        .expect("a sent event");
    lines[victim] = {
        use base64::Engine;
        let bogus = base64::engine::general_purpose::STANDARD.encode(b"a0001 NONSENSE\r\n");
        format!("{{\"dir\":\"sent\",\"bytes\":\"{bogus}\"}}")
    };
    std::fs::write(&path, lines.join("\n")).unwrap();

    let offline = World::offline();
    let run = offline.run(&["--json", "debug", "replay", path.to_str().unwrap()]);
    assert_ne!(run.status, 0, "a divergence should fail the replay");
    assert!(
        run.stderr.contains("diverged"),
        "the failure should say what happened: {}",
        run.stderr
    );
}

/// §14.2: a stale transcript is a test that passes about a server that no
/// longer exists. Refusing one is the suite's call, so the age is reported and
/// `--max-age-days` enforces it.
#[test]
fn a_stale_transcript_can_be_refused() {
    let live = World::live();
    let path = live.dir.join("sync.ndjson");
    assert_eq!(live.record(&path, "first sync").status, 0);

    // Backdate the recording by a year.
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut header: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    header["recorded_at"] = serde_json::Value::String("2025-09-02T12:00:00Z".into());
    lines[0] = serde_json::to_string(&header).unwrap();
    std::fs::write(&path, lines.join("\n")).unwrap();

    let offline = World::offline();

    // Reported, and tolerated by default.
    let run = offline.run(&["--json", "debug", "replay", path.to_str().unwrap()]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    let payload: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
    assert!(
        payload["age_days"].as_i64().unwrap() >= 364,
        "the age should be reported: {payload}"
    );

    // And refused when the suite says so.
    let strict = World::offline();
    let run = strict.run(&[
        "--json",
        "debug",
        "replay",
        path.to_str().unwrap(),
        "--max-age-days",
        "30",
    ]);
    assert_ne!(run.status, 0);
    assert!(
        run.stderr.contains("re-record"),
        "the refusal should say what to do: {}",
        run.stderr
    );
}

/// A transcript is a file that gets committed, so the credential must not be in
/// it. Redaction keeps the byte structure so the replayed run still exercises
/// the same parsing.
#[test]
fn a_transcript_carries_no_credential() {
    let w = World::live();
    let path = w.dir.join("auth.ndjson");
    assert_eq!(w.record(&path, "first sync").status, 0);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("testpass"),
        "the password appears verbatim in the transcript"
    );

    // Decoded too — base64 would hide it from the check above.
    use base64::Engine;
    for line in text.lines().skip(1) {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(event["bytes"].as_str().unwrap())
            .unwrap();
        let decoded = String::from_utf8_lossy(&bytes);
        assert!(
            !decoded.contains("testpass"),
            "the password is in a transcript event: {decoded}"
        );
        // The base64 of "\0peter@example.com\0testpass" must not survive either.
        assert!(
            !decoded.contains("dGVzdHBhc3M"),
            "an encoded credential survived redaction: {decoded}"
        );
    }
}

/// Replaying twice must produce the same store both times — a replay that
/// depended on anything ambient would not.
#[test]
fn replaying_is_deterministic() {
    let live = World::live();
    let path = live.dir.join("sync.ndjson");
    assert_eq!(live.record(&path, "first sync").status, 0);

    let a = World::offline();
    assert_eq!(
        a.run(&["--json", "debug", "replay", path.to_str().unwrap()])
            .status,
        0
    );
    let b = World::offline();
    assert_eq!(
        b.run(&["--json", "debug", "replay", path.to_str().unwrap()])
            .status,
        0
    );
    assert_eq!(a.dump(), b.dump());
}

/// Every transcript committed under `tests/transcripts/` must still replay.
///
/// This is where L1 earns its place as a PR gate. The tests above record and
/// replay in one run, which proves the *mechanism*; these files pin the actual
/// conversation, so an engine change that alters what is sent fails here and
/// has to be re-recorded on purpose rather than drifting.
#[test]
fn every_committed_transcript_still_replays() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/transcripts");
    let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ndjson"))
        .collect();

    assert!(
        !entries.is_empty(),
        "no transcripts in {} — L1 has nothing to gate on",
        dir.display()
    );

    for path in entries {
        let w = World::offline();
        let run = w.run(&["--json", "debug", "replay", path.to_str().unwrap()]);
        assert_eq!(
            run.status,
            0,
            "{} no longer replays:\n{}\n{}",
            path.display(),
            run.stdout,
            run.stderr
        );

        let payload: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
        assert!(
            payload["diverged"].is_null(),
            "{} diverged: {}",
            path.display(),
            payload["diverged"]
        );
        // A recording that synced nothing would pass vacuously.
        assert!(
            payload["synced"][0]["messages_added"].as_u64().unwrap_or(0) > 0
                || payload["synced"][0]["folders_examined"]
                    .as_u64()
                    .unwrap_or(0)
                    > 0,
            "{} replayed but did nothing",
            path.display()
        );
    }
}

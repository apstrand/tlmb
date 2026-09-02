//! **L3, the IMAP side** (§14.2): the recovery paths real servers will not
//! produce.
//!
//! > Every recovery claim in §5.3 has a corresponding L3 scenario.
//!
//! The governing property across all of these is the same: a server behaving
//! badly may cost a sync pass, but it may never lose mail, hang, corrupt the
//! store, or leave it unopenable. A failed pass is recoverable; a store that
//! disagrees with itself is not.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mailbox_l3::{ImapFault, ImapServer, Message};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
    server: ImapServer,
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

impl World {
    fn new(messages: Vec<Message>) -> World {
        let server = ImapServer::start(messages).expect("start the L3 IMAP server");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-l3i-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\n\
                 address = \"peter@example.com\"\n\
                 imap_host = \"127.0.0.1\"\n\
                 imap_port = {}\n\
                 imap_security = \"none\"\n\
                 require_tls = false\n",
                server.port()
            ),
        )
        .expect("write config");

        World {
            state: dir.join("state"),
            config,
            dir,
            server,
        }
    }

    fn three() -> World {
        World::new(vec![
            Message::simple(1, "First", "one@example.com"),
            Message::simple(2, "Second", "two@example.com"),
            Message::simple(3, "Third", "three@example.com"),
        ])
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "l3imap")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            // A short timeout so a hang shows up as a failing test rather than
            // as a suite that never finishes.
            .env("MAILBOX_TEST", "1")
            .output()
            .expect("run the binary");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn sync(&self) -> Run {
        self.run(&["--json", "--timeout", "5000", "sync", "--once"])
    }

    /// The subjects the store holds, in id order — stable regardless of what
    /// the server did.
    fn stored_subjects(&self) -> Vec<String> {
        let run = self.run(&["debug", "store-dump"]);
        assert_eq!(run.status, 0, "the store is unreadable:\n{}", run.stderr);
        let dump: serde_json::Value = serde_json::from_str(&run.stdout).expect("dump");
        let mut subjects: Vec<String> = dump["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["subject"].as_str().unwrap_or_default().to_string())
            .collect();
        subjects.sort();
        subjects
    }

    /// The store must open and its derived state must be readable. This is the
    /// invariant every scenario below shares.
    fn assert_store_is_sound(&self, context: &str) {
        let run = self.run(&["debug", "store-dump"]);
        assert_eq!(
            run.status, 0,
            "{context}: the store will not open:\n{}",
            run.stderr
        );
        let dump: serde_json::Value =
            serde_json::from_str(&run.stdout).unwrap_or_else(|e| panic!("{context}: {e}"));

        // Every message belongs to a thread and sits in a folder: the two
        // derived values a torn write would break.
        for m in dump["messages"].as_array().unwrap() {
            assert!(
                m["thread_id"].is_string(),
                "{context}: a message has no thread: {m}"
            );
            assert!(
                !m["folder"].as_str().unwrap_or_default().is_empty(),
                "{context}: a message has no folder: {m}"
            );
        }
        // And `ls` works, which is what a user would try next.
        let ls = self.run(&["--json", "ls"]);
        assert_eq!(ls.status, 0, "{context}: ls failed:\n{}", ls.stderr);
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The baseline: with no fault armed, the L3 server behaves and everything
/// syncs. Without this, a scenario that "passes" might be passing because
/// nothing works.
#[test]
fn the_unfaulted_server_syncs_normally() {
    let w = World::three();
    let run = w.sync();
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(
        w.stored_subjects(),
        vec!["First".to_string(), "Second".into(), "Third".into()]
    );
    w.assert_store_is_sound("baseline");
}

/// A literal that announces more bytes than it sends, then the connection
/// closes. The client must not hang waiting for the rest, and must not keep a
/// half-read message.
#[test]
fn a_truncated_literal_does_not_hang_or_store_half_a_message() {
    let w = World::three();
    w.server.set_fault(ImapFault::TruncatedLiteral);

    let run = w.sync();
    // A failed pass is fine. A hang is not, and the harness timeout catches it.
    assert_ne!(
        run.status, 0,
        "a truncated literal should not report success"
    );
    w.assert_store_is_sound("truncated literal");
    assert!(
        w.stored_subjects().is_empty(),
        "half a message must not be stored: {:?}",
        w.stored_subjects()
    );

    // And the next pass, against a well-behaved server, recovers completely.
    w.server.set_fault(ImapFault::None);
    let run = w.sync();
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(w.stored_subjects().len(), 3);
}

/// A literal count that disagrees with its payload. The parser resynchronizes
/// onto the middle of a message body and sees garbage where a response should
/// be — the case §14.2 names explicitly.
#[test]
fn a_literal_count_that_lies_is_survived() {
    let w = World::three();
    w.server.set_fault(ImapFault::LiteralCountTooSmall);

    let run = w.sync();
    w.assert_store_is_sound("lying literal count");
    // Whatever the pass decided, the store must not contain a message built
    // from misframed bytes.
    for subject in w.stored_subjects() {
        assert!(
            ["First", "Second", "Third"].contains(&subject.as_str()),
            "a misframed response produced a bogus message: {subject:?}"
        );
    }
    let _ = run;

    w.server.set_fault(ImapFault::None);
    assert_eq!(w.sync().status, 0);
    assert_eq!(w.stored_subjects().len(), 3);
}

/// §5.3: a `UIDVALIDITY` change invalidates every UID we hold and forces a
/// rescan. Here it lands *mid-session*, between one `EXAMINE` and the next,
/// which is the case a between-passes test would miss.
#[test]
fn a_uidvalidity_bump_mid_session_is_recovered_from() {
    let w = World::three();
    assert_eq!(w.sync().status, 0);
    assert_eq!(w.stored_subjects().len(), 3);
    let before = w.server.uidvalidity();

    // A queued write makes the next pass select the folder twice: once to push
    // the operation, once to pull. Arming the bump for the second select is
    // what puts it genuinely *mid-pass* rather than between passes — which is
    // the case L2's delete-and-recreate already covers.
    let id = {
        let run = w.run(&["--json", "ls", "-n", "1"]);
        let v: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
        v["messages"][0]["id"].as_str().expect("an id").to_string()
    };
    let flagged = w.run(&["--json", "flag", &id, "+flagged"]);
    assert_eq!(flagged.status, 0, "{}", flagged.stderr);

    w.server
        .set_fault(ImapFault::BumpUidValidityAfterFirstSelect);
    w.server.reset_counters();

    let _ = w.sync();
    w.assert_store_is_sound("uidvalidity bump");

    w.server.set_fault(ImapFault::None);
    assert_eq!(w.sync().status, 0);
    assert_ne!(
        w.server.uidvalidity(),
        before,
        "the bump should have landed"
    );

    // §5.2's whole point: the messages come back with the same identities, so a
    // rescan costs bandwidth and not history.
    assert_eq!(
        w.stored_subjects(),
        vec!["First".to_string(), "Second".into(), "Third".into()]
    );
}

/// `BYE` partway through a `FETCH`. Everything already received is good; the
/// rest must be re-fetched on the next pass rather than silently dropped.
#[test]
fn a_bye_during_fetch_loses_nothing() {
    let w = World::three();
    w.server.set_fault(ImapFault::ByeDuringFetch);

    let _ = w.sync();
    w.assert_store_is_sound("bye during fetch");

    w.server.set_fault(ImapFault::None);
    assert_eq!(w.sync().status, 0);
    assert_eq!(
        w.stored_subjects(),
        vec!["First".to_string(), "Second".into(), "Third".into()],
        "the interrupted batch should have been completed, not abandoned"
    );
}

/// A `CAPABILITY` advertising `QRESYNC` that then refuses to `ENABLE` it.
///
/// The client must fall back rather than assume the extension it was promised —
/// and, crucially, must still sync.
#[test]
fn qresync_advertised_then_refused_falls_back() {
    let w = World::three();
    w.server.set_fault(ImapFault::QresyncAdvertisedThenRejected);

    let run = w.sync();
    assert_eq!(
        run.status, 0,
        "a refused ENABLE must not fail the sync:\n{}",
        run.stderr
    );
    assert_eq!(w.stored_subjects().len(), 3);
    w.assert_store_is_sound("qresync refused at ENABLE");
}

/// The nastier variant: `ENABLE` succeeds, and then the `EXAMINE` carrying
/// QRESYNC parameters is rejected. The client has already committed to the
/// extension by that point.
#[test]
fn qresync_enabled_then_rejected_at_select_still_syncs() {
    let w = World::three();
    // A first clean pass, so the second has a HIGHESTMODSEQ to send.
    assert_eq!(w.sync().status, 0);

    w.server
        .set_fault(ImapFault::QresyncEnabledThenSelectRejected);
    let _ = w.sync();
    w.assert_store_is_sound("qresync rejected at SELECT");

    w.server.set_fault(ImapFault::None);
    assert_eq!(w.sync().status, 0);
    assert_eq!(w.stored_subjects().len(), 3);
}

/// The connection simply vanishes partway through the conversation. §5.3's
/// reconnect promise, at every point a pass can be cut.
#[test]
fn a_connection_dropped_at_any_point_is_recovered_from() {
    for after in [1u32, 2, 3, 4, 5, 6] {
        let w = World::three();
        w.server.set_fault(ImapFault::DropAfterCommands(after));

        let _ = w.sync();
        w.assert_store_is_sound(&format!("drop after {after} commands"));

        w.server.set_fault(ImapFault::None);
        let run = w.sync();
        assert_eq!(
            run.status, 0,
            "drop after {after}: the recovery pass failed:\n{}",
            run.stderr
        );
        assert_eq!(
            w.stored_subjects().len(),
            3,
            "drop after {after}: mail was lost"
        );
    }
}

/// A slow server must hit `--timeout` rather than hanging the caller.
#[test]
fn a_slow_server_times_out() {
    let w = World::three();
    w.server.set_fault(ImapFault::Delay(Duration::from_secs(3)));

    let started = std::time::Instant::now();
    let run = w.run(&["--json", "--timeout", "700", "sync", "--once"]);
    let elapsed = started.elapsed();

    assert_ne!(run.status, 0, "a stalled server should not report success");
    assert!(
        elapsed < Duration::from_secs(20),
        "the client hung for {elapsed:?} instead of timing out"
    );
    w.assert_store_is_sound("slow server");
}

/// Mail that arrives while the server is misbehaving must still turn up once it
/// stops — the sync engine must not decide it has already seen everything.
#[test]
fn messages_added_during_a_fault_arrive_afterwards() {
    let w = World::three();
    assert_eq!(w.sync().status, 0);
    assert_eq!(w.stored_subjects().len(), 3);

    w.server.set_messages(vec![
        Message::simple(1, "First", "one@example.com"),
        Message::simple(2, "Second", "two@example.com"),
        Message::simple(3, "Third", "three@example.com"),
        Message::simple(4, "Fourth", "four@example.com"),
    ]);
    w.server.set_fault(ImapFault::DropAfterCommands(3));
    let _ = w.sync();

    w.server.set_fault(ImapFault::None);
    assert_eq!(w.sync().status, 0);
    assert!(
        w.stored_subjects().contains(&"Fourth".to_string()),
        "the message that arrived during the fault never turned up: {:?}",
        w.stored_subjects()
    );
}

// ---- M3: offline mode and lazy bodies (§5.1) ----

impl World {
    /// Rewrites the account's `offline` policy.
    fn set_offline(&self, mode: &str) {
        let text = std::fs::read_to_string(&self.config).unwrap();
        let stripped: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("offline"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&self.config, format!("{stripped}\noffline = \"{mode}\"\n")).unwrap();
    }

    /// Whether the store holds the message's raw bytes.
    fn bodies_cached(&self) -> usize {
        let run = self.run(&["debug", "store-dump"]);
        assert_eq!(run.status, 0, "{}", run.stderr);
        let dump: serde_json::Value = serde_json::from_str(&run.stdout).expect("dump");
        dump["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["body_cached"] == true)
            .count()
    }
}

/// §5.1: "headers and structure for everything, bodies for what you read plus a
/// configurable prefetch window". With the window off, a sync brings headers
/// and no bodies.
#[test]
fn offline_none_syncs_headers_without_bodies() {
    let w = World::three();
    w.set_offline("none");

    assert_eq!(w.sync().status, 0);
    assert_eq!(w.stored_subjects().len(), 3, "headers arrive regardless");
    assert_eq!(w.bodies_cached(), 0, "no body should have been fetched");
}

/// And "bodies for what you read": opening one fetches it.
#[test]
fn opening_a_message_fetches_its_body() {
    let w = World::three();
    w.set_offline("none");
    assert_eq!(w.sync().status, 0);
    assert_eq!(w.bodies_cached(), 0);

    let id = {
        let run = w.run(&["--json", "ls", "-n", "1"]);
        let v: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
        v["messages"][0]["id"].as_str().unwrap().to_string()
    };

    let shown = w.run(&["--json", "show", &id]);
    assert_eq!(shown.status, 0, "{}", shown.stderr);
    let detail: serde_json::Value = serde_json::from_str(&shown.stdout).expect("JSON");
    assert!(
        detail["message"]["body"]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Body of"),
        "the body should have been fetched on demand: {}",
        detail["message"]["body"]
    );

    assert_eq!(w.bodies_cached(), 1, "and cached for next time");
}

/// `--offline` means what it says: show what is cached and go nowhere.
#[test]
fn show_offline_never_fetches() {
    let w = World::three();
    w.set_offline("none");
    assert_eq!(w.sync().status, 0);

    let id = {
        let run = w.run(&["--json", "ls", "-n", "1"]);
        let v: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
        v["messages"][0]["id"].as_str().unwrap().to_string()
    };

    let shown = w.run(&["--json", "show", &id, "--offline"]);
    assert_eq!(
        shown.status, 0,
        "a missing body is not an error: {}",
        shown.stderr
    );
    let detail: serde_json::Value = serde_json::from_str(&shown.stdout).expect("JSON");
    assert!(
        detail["message"]["body"].is_null(),
        "an offline show must not have fetched anything"
    );
    // The headers are still there, which is the point of fetching them eagerly.
    assert!(!detail["message"]["subject"].as_str().unwrap().is_empty());
    assert_eq!(w.bodies_cached(), 0);
}

/// §5.1's `offline = "full"`: fetch everything, for offline use.
///
/// This is a regression test with teeth: the setting used to be expressed as a
/// prefetch window near i64::MAX, which underflowed on subtraction and, having
/// wrapped, matched nothing — so the setting that means "download everything"
/// downloaded nothing at all.
#[test]
fn offline_full_fetches_every_body() {
    let w = World::three();
    w.set_offline("full");

    assert_eq!(w.sync().status, 0);
    assert_eq!(
        w.bodies_cached(),
        3,
        "offline = full should have fetched every body"
    );
}

/// The default window fetches recent mail eagerly. The L3 corpus is dated
/// inside it, so all three arrive without anyone opening them.
#[test]
fn the_default_window_prefetches_recent_bodies() {
    let w = World::three();
    w.set_offline("recent");

    assert_eq!(w.sync().status, 0);
    assert_eq!(w.bodies_cached(), 3);
}

/// `attach` lists parts from what is cached, and fetches the message if it has
/// to (§6.1).
#[test]
fn attach_lists_and_extracts_from_a_lazily_fetched_message() {
    let w = World::three();
    w.set_offline("none");
    assert_eq!(w.sync().status, 0);

    let id = {
        let run = w.run(&["--json", "ls", "-n", "1"]);
        let v: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
        v["messages"][0]["id"].as_str().unwrap().to_string()
    };

    // The L3 messages are single-part, so part 1 is the whole message body.
    let listed = w.run(&["--json", "attach", &id]);
    assert_eq!(listed.status, 0, "{}", listed.stderr);
    let payload: serde_json::Value = serde_json::from_str(&listed.stdout).expect("JSON");
    assert_eq!(payload["id"], id);
    // Fetching happened as a side effect of needing the structure.
    assert_eq!(w.bodies_cached(), 1);

    let out = w.run(&["attach", &id, "1", "--offline"]);
    assert_eq!(out.status, 0, "{}", out.stderr);
    assert!(
        out.stdout.contains("Body of"),
        "the part should have come out on stdout: {:?}",
        out.stdout
    );
}

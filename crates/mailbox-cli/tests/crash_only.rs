//! **The crash-only oracle** (§14.3), in its first form.
//!
//! > A supervisor `SIGKILL`s a process at a randomized trace event, restarts
//! > it, and asserts: no lost outbox entry, no duplicate send, no torn
//! > transaction, no store that fails to open.
//!
//! The supervisor is this file. It runs a clean send once to learn which trace
//! events a send actually produces, then replays the whole scenario killing at
//! each one in turn — not a random sample, because the interesting events are
//! few enough to take all of them and a randomized suite that goes green is a
//! suite that has not told you which case it skipped.
//!
//! The one exception §16 carves out is checked exactly rather than waved at:
//! the server is asked how many messages it held *before* the restart, so a
//! second delivery is only tolerated when the first one had already happened.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use mailbox_l3::SmtpServer;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
    server: SmtpServer,
}

impl World {
    fn new() -> World {
        let server = SmtpServer::start().expect("start the L3 server");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-crash-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\n\
                 address = \"peter@example.com\"\n\
                 imap_host = \"127.0.0.1\"\n\
                 imap_port = 1\n\
                 imap_security = \"none\"\n\
                 smtp_host = \"127.0.0.1\"\n\
                 smtp_port = {}\n\
                 smtp_security = \"none\"\n\
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

    /// Runs the binary, optionally killing it at a trace event.
    fn run(&self, args: &[&str], crash_at: Option<&str>, trace: Option<&PathBuf>) -> Outcome {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "crash")
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
            .env_remove("MAILBOX_AGENT")
            .env_remove("MAILBOX_CRASH_AT");
        if let Some(event) = crash_at {
            cmd.env("MAILBOX_CRASH_AT", event);
        }
        if let Some(path) = trace {
            cmd.arg("--trace").arg(path);
        }

        let out = cmd.output().expect("run the binary");
        Outcome {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.run(&full, None, None);
        assert_eq!(
            out.status,
            Some(0),
            "`mailbox {}` failed:\n{}\n{}",
            full.join(" "),
            out.stdout,
            out.stderr
        );
        serde_json::from_str(&out.stdout).expect("JSON")
    }

    fn outbox(&self) -> Vec<serde_json::Value> {
        self.json(&["outbox", "list"])["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Outcome {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Outcome {
    /// A process killed by a signal has no exit code.
    fn was_killed(&self) -> bool {
        self.status.is_none()
    }
}

/// The trace events one clean send produces, in order and deduplicated.
///
/// Learned from a real run rather than hardcoded: a list written by hand goes
/// stale the moment someone adds an event, and the suite would keep passing
/// while quietly testing less.
fn events_during_a_send() -> Vec<String> {
    let w = World::new();
    w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Mapping the events",
        "--body",
        "Hello.",
    ]);

    let trace = w.dir.join("clean.ndjson");
    let out = w.run(&["--json", "outbox", "flush"], None, Some(&trace));
    assert_eq!(out.status, Some(0), "{}", out.stderr);

    let text = std::fs::read_to_string(&trace).expect("a trace");
    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    for line in text.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(name) = event["event"].as_str() else {
            continue;
        };
        if seen.insert(name.to_string()) {
            ordered.push(name.to_string());
        }
    }
    assert!(
        ordered.iter().any(|e| e == "smtp.data"),
        "the mapping run should have reached the body: {ordered:?}"
    );
    ordered
}

/// The oracle. For every event a send emits: queue a message, kill the flush at
/// that event, restart, and check the four invariants §14.3 names.
#[test]
fn a_send_survives_a_kill_at_every_trace_event() {
    let events = events_during_a_send();
    assert!(events.len() > 5, "suspiciously few events: {events:?}");

    for event in &events {
        let w = World::new();
        let queued = w.json(&[
            "send",
            "--to",
            "alice@example.com",
            "--subject",
            "Crash test",
            "--body",
            "Hello.",
        ]);
        let id = queued["id"].as_i64().expect("an outbox id");

        // Kill the flush at this event.
        let trace = w.dir.join("crash.ndjson");
        let killed = w.run(&["--json", "outbox", "flush"], Some(event), Some(&trace));

        // Not every event is reached on every path — `smtp.disconnect` fires
        // after the work is done, for instance — so a clean exit here is fine
        // and simply means this run had nothing to interrupt.
        let delivered_before_restart = w.server.received().len();

        // --- invariant: the store still opens, and the entry is still there.
        let entries = w.outbox();
        assert_eq!(
            entries.len(),
            1,
            "killed at {event}: the outbox should hold exactly one entry, got {entries:?}"
        );
        assert_eq!(
            entries[0]["id"].as_i64(),
            Some(id),
            "killed at {event}: the entry changed identity"
        );

        // --- invariant: no torn transaction. The dump is the store's own
        // consistency check, and it reads every derived table.
        let dump = w.run(&["debug", "store-dump"], None, None);
        assert_eq!(
            dump.status,
            Some(0),
            "killed at {event}: the store is unreadable:\n{}",
            dump.stderr
        );

        // --- the restart.
        let restarted = w.run(&["--json", "outbox", "flush"], None, None);
        assert_eq!(
            restarted.status,
            Some(0),
            "killed at {event}: the restart failed:\n{}",
            restarted.stderr
        );

        let delivered_after = w.server.received().len();

        // --- invariant: nothing was lost.
        assert!(
            delivered_after >= 1,
            "killed at {event}: the message was never delivered"
        );
        let final_state = w.outbox()[0]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            final_state, "sent",
            "killed at {event}: the entry ended up {final_state}"
        );

        // --- invariant: no duplicate send, except the one window §16 accepts.
        //
        // Checked exactly rather than by naming events: the server is asked how
        // many copies it held *before* the restart, so a second delivery is
        // only tolerated when the first had already happened and the crash beat
        // the record of it.
        if delivered_before_restart == 0 {
            assert_eq!(
                delivered_after, 1,
                "killed at {event}: delivered {delivered_after} times having delivered none before the restart"
            );
        } else {
            assert!(
                delivered_after <= delivered_before_restart + 1,
                "killed at {event}: {delivered_before_restart} before the restart, {delivered_after} after — more than one retry"
            );
        }

        if killed.was_killed() {
            // The trace is what makes the failure above diagnosable, so it has
            // to survive the kill. Flushing per event is what buys that.
            let text = std::fs::read_to_string(&trace).unwrap_or_default();
            assert!(
                text.lines().any(|l| l.contains(event)),
                "killed at {event}: the trace does not record the event it died on"
            );
        }
    }
}

/// The queue side of the same property: a kill between the local write and the
/// push must not lose the operation, and must not apply it twice.
#[test]
fn a_queued_write_survives_a_kill_during_enqueue() {
    for event in ["store.txn.begin", "store.txn.commit"] {
        let w = World::new();
        let killed = w.run(
            &[
                "--json",
                "send",
                "--to",
                "alice@example.com",
                "--subject",
                "Interrupted enqueue",
                "--body",
                "Hello.",
            ],
            Some(event),
            None,
        );

        // The store must open afterwards whatever happened.
        let entries = w.outbox();
        assert!(
            entries.len() <= 1,
            "killed at {event}: enqueue left {} entries",
            entries.len()
        );

        // Whatever survived, flushing it delivers at most one copy: a
        // half-written entry must not become a second message.
        let out = w.run(&["--json", "outbox", "flush"], None, None);
        assert_eq!(out.status, Some(0), "killed at {event}: {}", out.stderr);
        assert!(
            w.server.received().len() <= 1,
            "killed at {event}: an interrupted enqueue produced {} deliveries",
            w.server.received().len()
        );

        if killed.was_killed() && !entries.is_empty() {
            assert_eq!(w.outbox()[0]["state"], "sent");
        }
    }
}

/// The hook itself: `MAILBOX_CRASH_AT` must actually kill, and must not fire on
/// an event that never happens — otherwise every test above would be vacuous.
#[test]
fn the_crash_hook_kills_only_where_it_is_told() {
    let w = World::new();
    w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Hook check",
        "--body",
        "Hello.",
    ]);

    let killed = w.run(&["--json", "outbox", "flush"], Some("smtp.connect"), None);
    assert!(
        killed.was_killed(),
        "the hook did not kill: exit {:?}",
        killed.status
    );
    assert!(w.server.received().is_empty(), "it died before sending");

    // An event this command never emits must leave it alone.
    let clean = w.run(
        &["--json", "outbox", "flush"],
        Some("sync.uidvalidity_reset"),
        None,
    );
    assert_eq!(clean.status, Some(0), "{}", clean.stderr);
    assert_eq!(w.server.received().len(), 1);
}

/// The `:n` form, so a test can kill at the third commit rather than the first.
#[test]
fn the_crash_hook_can_target_a_later_occurrence() {
    let w = World::new();
    w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Occurrence",
        "--body",
        "Hello.",
    ]);

    // A flush emits a handful of SMTP sends; killing at the first and at the
    // fourth must reach different points.
    let first = w.dir.join("first.ndjson");
    w.run(&["outbox", "flush"], Some("smtp.send:1"), Some(&first));
    let early = std::fs::read_to_string(&first).unwrap_or_default();

    let w2 = World::new();
    w2.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Occurrence",
        "--body",
        "Hello.",
    ]);
    let later = w2.dir.join("later.ndjson");
    w2.run(&["outbox", "flush"], Some("smtp.send:4"), Some(&later));
    let late = std::fs::read_to_string(&later).unwrap_or_default();

    assert!(
        late.lines().count() > early.lines().count(),
        "`:4` should have got further than `:1` ({} vs {} events)",
        late.lines().count(),
        early.lines().count()
    );
}

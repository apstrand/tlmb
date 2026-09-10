//! The change feed and `mailbox watch` (§6.4).
//!
//! What is being pinned here is the promise an agent's loop rests on: an event
//! for every change, in order, exactly once, with a cursor that resumes where
//! it left off. A feed that drops one event is worse than no feed at all —
//! the caller has no way to notice.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

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
to = ["peter@example.com"]
subject = "Invoice 42"
date = "2026-08-01T10:00:00Z"
message_id = "<inv42@example.com>"
body = "Please pay the invoice."

[[message]]
name = "two"
folder = "INBOX"
from = "Bob <bob@example.com>"
to = ["peter@example.com"]
subject = "Lunch"
date = "2026-08-01T12:00:00Z"
message_id = "<lunch@example.com>"
flags = ["\\Seen"]
body = "One?"
"#;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Cli {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Cli {
    fn new() -> Cli {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-events-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");

        let cli = Cli {
            state: dir.join("state"),
            config,
            dir,
        };
        let manifest = cli.dir.join("fixture.toml");
        std::fs::write(&manifest, FIXTURE).expect("write fixture");
        let seeded = cli.run(&["debug", "fixture", "load", manifest.to_str().unwrap()]);
        assert_eq!(seeded.status, 0, "seeding failed: {}", seeded.stderr);
        cli
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_SEED", "events")
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

    fn run(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().expect("run the binary");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let run = self.run(&full);
        assert_eq!(run.status, 0, "`{}` failed: {}", full.join(" "), run.stderr);
        serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("expected one JSON document, got {e}:\n{}", run.stdout))
    }

    /// Everything in the feed, from the beginning of time.
    fn feed(&self) -> Vec<serde_json::Value> {
        let doc = self.ok(&["watch", "--drain", "--cursor", "test:0"]);
        doc["changes"].as_array().cloned().unwrap_or_default()
    }

    fn id(&self, message_id: &str) -> String {
        let dump: serde_json::Value =
            serde_json::from_str(&self.run(&["debug", "store-dump"]).stdout).expect("dump");
        dump["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["message_id"].as_str() == Some(message_id))
            .unwrap_or_else(|| panic!("no message with Message-ID {message_id}"))["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn kinds(changes: &[serde_json::Value]) -> Vec<String> {
    changes
        .iter()
        .map(|c| c["event"].as_str().unwrap_or_default().to_string())
        .collect()
}

// ---- what lands in the feed ----

#[test]
fn arriving_mail_is_one_new_event_each() {
    let cli = Cli::new();
    let feed = cli.feed();
    assert_eq!(kinds(&feed), ["new", "new"], "one per message: {feed:#?}");
    let subjects: Vec<&str> = feed
        .iter()
        .filter_map(|c| c["detail"]["subject"].as_str())
        .collect();
    assert!(subjects.contains(&"Invoice 42"), "{subjects:?}");
    // The folder it arrived in, which is what a rule acts on.
    assert_eq!(feed[0]["folder"], "INBOX");
    // Not local: the mail came from the server, and an agent must be able to
    // tell its own writes from everyone else's.
    assert_eq!(feed[0]["local"], false);
}

#[test]
fn a_local_flag_is_a_local_event_carrying_the_flags_after_it() {
    let cli = Cli::new();
    let id = cli.id("inv42@example.com");
    let before = cli.feed().len();
    cli.run(&["flag", &id, "+seen"]);

    let feed = cli.feed();
    let event = &feed[before];
    assert_eq!(event["event"], "flag");
    assert_eq!(event["message"], id);
    assert_eq!(event["local"], true);
    assert_eq!(
        event["detail"]["flags"],
        serde_json::json!(["seen"]),
        "the flags after the change, not the delta: {event:#?}"
    );
}

#[test]
fn a_flag_that_changes_nothing_says_nothing() {
    let cli = Cli::new();
    // `two` is already seen in the fixture.
    let id = cli.id("lunch@example.com");
    let before = cli.feed().len();
    let run = cli.run(&["flag", &id, "+seen"]);
    assert_eq!(run.status, 0);
    assert_eq!(
        cli.feed().len(),
        before,
        "a no-op write must not wake every watcher"
    );
}

#[test]
fn a_move_names_where_it_came_from() {
    let cli = Cli::new();
    let id = cli.id("inv42@example.com");
    let before = cli.feed().len();
    let run = cli.run(&["mv", &id, "Archive"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let feed = cli.feed();
    let event = &feed[before];
    assert_eq!(event["event"], "move");
    assert_eq!(event["folder"], "Archive", "where it went");
    assert_eq!(event["detail"]["from"], "INBOX", "where it was");
}

// ---- the cursor ----

#[test]
fn a_cursor_resumes_after_what_it_has_seen() {
    let cli = Cli::new();
    let first = cli.ok(&["watch", "--drain", "--cursor", "test:0"]);
    let cursor = first["cursor"].as_str().expect("a cursor").to_string();
    assert!(!first["changes"].as_array().unwrap().is_empty());

    let id = cli.id("inv42@example.com");
    cli.run(&["flag", &id, "+flagged"]);

    let next = cli.ok(&["watch", "--drain", "--cursor", &cursor]);
    let changes = next["changes"].as_array().unwrap();
    assert_eq!(kinds(changes), ["flag"], "only what happened since");
    assert_ne!(next["cursor"], first["cursor"], "and it advanced");
}

#[test]
fn a_cursor_at_the_end_returns_nothing_and_stays_put() {
    let cli = Cli::new();
    let first = cli.ok(&["watch", "--drain", "--cursor", "test:0"]);
    let cursor = first["cursor"].as_str().unwrap().to_string();
    let again = cli.ok(&["watch", "--drain", "--cursor", &cursor]);
    assert!(again["changes"].as_array().unwrap().is_empty());
    assert_eq!(
        again["cursor"], first["cursor"],
        "a poll that found nothing must not lose its place"
    );
}

#[test]
fn without_a_cursor_a_watch_starts_from_now() {
    let cli = Cli::new();
    // The two arrivals are already in the feed, and a watcher starting today
    // is not interested in them.
    let doc = cli.ok(&["watch", "--drain"]);
    assert!(
        doc["changes"].as_array().unwrap().is_empty(),
        "history is what `--since` is for: {doc:#?}"
    );
}

#[test]
fn since_replays_history() {
    let cli = Cli::new();
    let doc = cli.ok(&["watch", "--drain", "--since", "1h"]);
    assert_eq!(kinds(doc["changes"].as_array().unwrap()), ["new", "new"]);
}

#[test]
fn a_nonsense_cursor_is_a_usage_error() {
    let cli = Cli::new();
    let run = cli.run(&["--json", "watch", "--drain", "--cursor", "nonsense"]);
    assert_eq!(run.status, 2, "{}", run.stdout);
}

// ---- filtering ----

#[test]
fn events_selects_kinds() {
    let cli = Cli::new();
    let id = cli.id("inv42@example.com");
    cli.run(&["flag", &id, "+flagged"]);

    let doc = cli.ok(&["watch", "--drain", "--cursor", "test:0", "--events", "flag"]);
    assert_eq!(kinds(doc["changes"].as_array().unwrap()), ["flag"]);

    let both = cli.ok(&[
        "watch", "--drain", "--cursor", "test:0", "--events", "new,flag",
    ]);
    assert_eq!(both["changes"].as_array().unwrap().len(), 3);
}

#[test]
fn an_unknown_event_name_is_an_error_rather_than_an_empty_stream() {
    let cli = Cli::new();
    let run = cli.run(&["--json", "watch", "--drain", "--events", "nwe"]);
    assert_eq!(
        run.status, 2,
        "a typo that silently matches nothing is the worst outcome: {}",
        run.stdout
    );
    let error: serde_json::Value = serde_json::from_str(&run.stderr).expect("a JSON error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("new"),
        "and it should say what the names are: {error}"
    );
}

// ---- streaming ----

#[test]
fn a_stream_emits_an_event_that_happens_while_it_waits() {
    let cli = Cli::new();
    let id = cli.id("inv42@example.com");

    let mut child = cli
        .command(&[
            "--ndjson",
            "watch",
            "--no-sync",
            "--max-events",
            "1",
            "--stop-after",
            "20",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn watch");

    // The `started` line is emitted before the first read of the feed, so
    // seeing it is what says the watcher is listening — no sleep required.
    let mut out = std::io::BufReader::new(child.stdout.take().expect("stdout"));
    let started = read_line(&mut out);
    assert_eq!(started["payload"]["event"], "started");
    assert_eq!(started["payload"]["syncing"], false, "--no-sync");

    let run = cli.run(&["flag", &id, "+flagged"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let event = read_line(&mut out);
    assert_eq!(event["payload"]["event"], "flag");
    assert_eq!(event["payload"]["message"], id);

    let status = child.wait().expect("watch exits after --max-events");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn a_quiet_stream_ends_when_it_is_told_to() {
    let cli = Cli::new();
    let run = cli.run(&["--ndjson", "watch", "--no-sync", "--stop-after", "0.2"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    // Just the `started` line: nothing happened.
    assert_eq!(run.stdout.trim().lines().count(), 1, "{}", run.stdout);
}

fn read_line(out: &mut impl std::io::BufRead) -> serde_json::Value {
    let mut line = String::new();
    let n = std::io::BufRead::read_line(out, &mut line).expect("read a line");
    assert!(n > 0, "the stream ended early");
    serde_json::from_str(&line).unwrap_or_else(|e| panic!("expected NDJSON, got {e}: {line}"))
}

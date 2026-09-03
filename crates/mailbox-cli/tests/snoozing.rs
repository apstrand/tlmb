//! Snooze end to end (§7): what leaves the inbox, what comes back, and what
//! the server is told about it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const FIXTURE: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[message]]
name = "one"
folder = "INBOX"
from = "Alice <alice@example.com>"
subject = "First"
date = "2026-08-01T10:00:00Z"
message_id = "<one@example.com>"
body = "one"

[[message]]
name = "two"
folder = "INBOX"
from = "Bob <bob@example.com>"
subject = "Second"
date = "2026-08-02T10:00:00Z"
message_id = "<two@example.com>"
body = "two"
"#;

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

impl Run {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("expected one JSON document, got {e}:\n{}", self.stdout))
    }
}

impl Cli {
    fn new() -> Cli {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-snooze-{}-{n}", std::process::id()));
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
        let seeded = cli.at(
            "2026-09-03T10:00:00Z",
            &["debug", "fixture", "load", manifest.to_str().unwrap()],
        );
        assert_eq!(seeded.status, 0, "seeding failed: {}", seeded.stderr);
        cli
    }

    /// Runs with the clock pinned, which is how "later" happens in a test
    /// without anything sleeping.
    fn at(&self, now: &str, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_CLOCK", now)
            .env("MAILBOX_DEVICE", "thinkpad")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("COLUMNS", "100")
            .env("NO_COLOR", "1")
            .env_remove("MAILBOX_AGENT")
            .output()
            .expect("run the binary");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn ids(&self, now: &str) -> Vec<String> {
        self.at(now, &["--json", "ls", "-n", "10"]).json()["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|m| m["subject"].as_str().unwrap().to_string())
            .collect()
    }

    fn id_of(&self, subject: &str) -> String {
        let listing = self
            .at("2026-09-03T10:00:00Z", &["--json", "ls", "-n", "10"])
            .json();
        listing["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["subject"] == subject)
            .unwrap_or_else(|| panic!("no message {subject}"))["id"]
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

#[test]
fn a_snoozed_message_leaves_the_inbox_and_comes_back_by_itself() {
    let cli = Cli::new();
    let id = cli.id_of("First");

    let done = cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "2h"]);
    assert_eq!(done.status, 0, "{}", done.stderr);

    assert_eq!(
        cli.ids("2026-09-03T11:00:00Z"),
        vec!["Second"],
        "still asleep an hour later"
    );

    // Nothing ran in between, and nothing swept anything: being awake is a
    // comparison at read time, so a laptop that was shut when the timer
    // expired shows the message the moment it is opened.
    assert_eq!(
        cli.ids("2026-09-03T12:00:01Z"),
        vec!["Second", "First"],
        "back in the inbox once the time has passed"
    );
}

/// Hiding applies to the default listing, which is what makes `ls is:snoozed`
/// mean anything. A snooze that filtered every search would be a way to lose
/// mail.
#[test]
fn an_explicit_query_still_finds_a_sleeping_message() {
    let cli = Cli::new();
    let id = cli.id_of("First");
    cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "2h"]);

    let found = cli
        .at("2026-09-03T11:00:00Z", &["--json", "search", "First"])
        .json();
    assert_eq!(found["messages"].as_array().unwrap().len(), 1, "{found:#}");

    let asleep = cli
        .at("2026-09-03T11:00:00Z", &["--json", "ls", "is:snoozed"])
        .json();
    assert_eq!(asleep["messages"][0]["subject"], "First", "{asleep:#}");
}

/// §7 asks the interface to say which device holds a snooze rather than imply
/// it is synced. That is a promise about output, so it is tested as one.
#[test]
fn every_listing_names_the_device_holding_the_timer() {
    let cli = Cli::new();
    let id = cli.id_of("First");

    let set = cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "2h"]);
    // The note goes to stderr, where every advisory does, so that piping the
    // result somewhere does not carry commentary with it.
    assert!(set.stderr.contains("thinkpad"), "{}", set.stderr);
    assert!(
        set.stderr
            .contains("comes back on the device that snoozed it"),
        "the limitation has to be stated: {}",
        set.stderr
    );

    let listed = cli.at("2026-09-03T11:00:00Z", &["snooze", "--list"]);
    assert!(listed.stdout.contains("thinkpad"), "{}", listed.stdout);

    let json = cli
        .at("2026-09-03T11:00:00Z", &["--json", "snooze", "--list"])
        .json();
    assert_eq!(json["snoozed"][0]["device"], "thinkpad", "{json:#}");
    assert_eq!(
        json["snoozed"][0]["until"], "2026-09-03T12:00:00Z",
        "{json:#}"
    );
}

#[test]
fn a_snooze_can_be_cleared_and_replaced() {
    let cli = Cli::new();
    let id = cli.id_of("First");

    cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "2h"]);
    // Re-snoozing replaces the deadline, which is what "snooze this again"
    // has to mean.
    cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "5h"]);
    let json = cli
        .at("2026-09-03T10:00:00Z", &["--json", "snooze", "--list"])
        .json();
    assert_eq!(json["snoozed"].as_array().unwrap().len(), 1, "{json:#}");
    assert_eq!(
        json["snoozed"][0]["until"], "2026-09-03T15:00:00Z",
        "{json:#}"
    );

    let cleared = cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "--clear"]);
    assert_eq!(cleared.status, 0, "{}", cleared.stderr);
    assert_eq!(cli.ids("2026-09-03T10:00:00Z").len(), 2, "back immediately");
}

/// §5.3: a snooze is not an IMAP concept. Inventing one — a keyword, a hidden
/// folder — would put a private decision into a mailbox shared with every
/// other client the user runs, so nothing may be queued for the server.
#[test]
fn nothing_about_a_snooze_is_queued_for_the_server() {
    let cli = Cli::new();
    let id = cli.id_of("First");
    cli.at("2026-09-03T10:00:00Z", &["snooze", &id, "2h"]);

    let status = cli.at("2026-09-03T10:00:00Z", &["--json", "status"]).json();
    let pending = status["queue"]["pending"].as_u64().unwrap_or(0);
    assert_eq!(pending, 0, "a snooze queued an operation: {status:#}");

    // And the flags are untouched: this is not a `\Seen` in disguise either.
    let shown = cli
        .at("2026-09-03T10:00:00Z", &["--json", "show", &id])
        .json();
    let flags = shown["message"]["flags"].as_array().unwrap();
    assert!(flags.is_empty(), "{shown:#}");
}

#[test]
fn several_messages_can_be_snoozed_at_once() {
    let cli = Cli::new();
    let one = cli.id_of("First");
    let two = cli.id_of("Second");

    let done = cli.at("2026-09-03T10:00:00Z", &["snooze", &one, &two, "tomorrow"]);
    assert_eq!(done.status, 0, "{}", done.stderr);
    assert!(cli.ids("2026-09-03T11:00:00Z").is_empty());
    assert_eq!(cli.ids("2026-09-04T01:00:00Z").len(), 2);
}

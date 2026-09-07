//! **L3**: the newest-first backfill of a large mailbox (§5.1).
//!
//! The case this exists for is an account with twenty years of mail. Fetching
//! all of it before the interface shows anything means staring at nothing for
//! an hour and losing the lot to one dropped connection, so a first sync takes
//! the newest chunk and walks backwards over later passes.
//!
//! Two properties matter and neither is visible from a single pass: that the
//! walk *finishes*, and that every interface says the mailbox is incomplete
//! while it is.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use mailbox_l3::{ImapServer, Message};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Comfortably more than one chunk, so the walk takes several passes.
const TOTAL: u32 = 5_000;

struct World {
    dir: PathBuf,
    config: PathBuf,
    #[allow(dead_code)]
    server: ImapServer,
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("expected JSON, got {e}:\n{}\n{}", self.stdout, self.stderr))
    }
}

impl World {
    fn new(count: u32) -> World {
        let messages: Vec<Message> = (1..=count)
            .map(|uid| {
                Message::simple(
                    uid,
                    &format!("Message {uid}"),
                    &format!("m{uid}@example.com"),
                )
            })
            .collect();
        let server = ImapServer::start(messages).expect("start the L3 server");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-backfill-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\naddress = \"peter@example.com\"\n\
                 imap_host = \"127.0.0.1\"\nimap_port = {}\n\
                 imap_security = \"none\"\nrequire_tls = false\n\
                 \n[notify]\nenabled = false\n",
                server.port()
            ),
        )
        .expect("write config");

        World {
            dir,
            config,
            server,
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", self.dir.join("state"))
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "backfill")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            .output()
            .expect("run");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn sync(&self) -> Run {
        let run = self.run(&["--json", "sync", "--once"]);
        assert_eq!(run.status, 0, "sync failed: {}", run.stderr);
        run
    }

    fn held(&self) -> u64 {
        self.run(&["--json", "status"]).json()["accounts"][0]["messages"]
            .as_u64()
            .unwrap_or(0)
    }

    fn coverage(&self) -> serde_json::Value {
        self.run(&["--json", "status"]).json()["accounts"][0]["coverage"].clone()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The point of the whole thing: after one pass there is mail to read, and it
/// is the *newest* mail rather than an arbitrary slice.
#[test]
fn the_first_pass_brings_back_the_newest_mail_and_stops() {
    let w = World::new(TOTAL);
    w.sync();

    let held = w.held();
    assert!(held > 0, "a first sync should leave something to read");
    assert!(
        held < TOTAL as u64,
        "a first sync should not fetch a whole archive: {held}"
    );

    // Newest *first*: what arrived is the top of the UID range, not an
    // arbitrary slice. Checked by UID rather than by date because every
    // message the L3 server generates carries the same `Date` — which is
    // exactly the case where an accidental date sort would look right.
    let listing = w.run(&["--json", "ls", "-n", "5000"]).json();
    let numbers: Vec<u32> = listing["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m["subject"].as_str())
        .filter_map(|s| s.strip_prefix("Message ")?.parse().ok())
        .collect();
    assert_eq!(numbers.len() as u64, held, "every held message should list");
    let lowest = numbers.iter().copied().min().expect("a message");
    assert!(
        lowest > TOTAL - held as u32,
        "the fetched slice should be the newest {held}, but it reaches down to {lowest}"
    );
    assert!(
        numbers.contains(&TOTAL),
        "and it should include the very newest message"
    );
}

/// And every listing says so while it is true.
#[test]
fn an_incomplete_store_says_so_everywhere_it_is_asked() {
    let w = World::new(TOTAL);
    w.sync();

    let listing = w.run(&["--json", "ls"]).json();
    assert_eq!(
        listing["partial"], true,
        "a listing drawn from part of a mailbox has to say so: {listing}"
    );

    let coverage = w.coverage();
    assert_eq!(coverage["complete"], false);
    assert!(
        coverage["remaining"].as_u64().unwrap_or(0) > 0,
        "{coverage}"
    );
    assert!(
        coverage["oldest"].is_string(),
        "how far back the store reaches is the number a person wants: {coverage}"
    );

    // And in the human output, after the rows rather than before them.
    let human = w.run(&["ls"]);
    assert!(
        human.stdout.contains("still fetching older mail")
            || human.stderr.contains("still fetching older mail"),
        "stdout:\n{}\nstderr:\n{}",
        human.stdout,
        human.stderr
    );
}

/// The walk finishes. This is the property a chunked backfill most easily gets
/// wrong: claiming a `HIGHESTMODSEQ` partway through sends every later pass
/// down the CONDSTORE delta path, which never lists the folder again — and the
/// backfill stops for good, a few thousand messages in, silently.
#[test]
fn repeated_passes_walk_back_to_the_beginning_and_then_stop() {
    let w = World::new(TOTAL);

    let mut passes = 0;
    let mut previous = 0;
    loop {
        w.sync();
        passes += 1;
        let held = w.held();
        assert!(
            held > previous,
            "pass {passes} made no progress: still {held} of {TOTAL}"
        );
        previous = held;
        if held >= TOTAL as u64 {
            break;
        }
        assert!(passes < 20, "the walk is not converging: {held} of {TOTAL}");
    }

    assert!(passes > 1, "this mailbox should have needed several passes");
    assert_eq!(w.held(), TOTAL as u64);

    // And once it is done it says so, and stays done.
    let coverage = w.coverage();
    assert_eq!(coverage["complete"], true, "{coverage}");
    assert_eq!(coverage["remaining"], 0);
    assert_eq!(w.run(&["--json", "ls"]).json()["partial"], false);

    let human = w.run(&["ls"]);
    assert!(
        !human.stdout.contains("still fetching"),
        "a complete mailbox should say nothing: {}",
        human.stdout
    );

    // A further pass must not undo it.
    w.sync();
    assert_eq!(w.coverage()["complete"], true);
}

/// A mailbox small enough to arrive whole is never called partial, and never
/// pays for any of this.
#[test]
fn a_small_mailbox_arrives_complete_in_one_pass() {
    let w = World::new(20);
    w.sync();

    assert_eq!(w.held(), 20);
    assert_eq!(w.coverage()["complete"], true);
    assert_eq!(w.run(&["--json", "ls"]).json()["partial"], false);
}

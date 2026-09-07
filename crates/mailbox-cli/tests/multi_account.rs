//! Several accounts as one mailbox (§1.5, §7), with no server involved.
//!
//! Fixtures seed each account's store directly (§14.1), so this suite runs in
//! milliseconds and needs neither Docker nor a network. What it checks is the
//! part that is ours rather than IMAP's: merging, ordering, paginating, and
//! routing a write back to the account that owns the message.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const WORK: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"
[[folder]]
path = "Archive"
special_use = "archive"

[[message]]
name = "numbers"
folder = "INBOX"
from = "Boss <boss@work.example>"
to = ["peter@work.example"]
subject = "Quarterly numbers"
date = "2026-08-30T09:00:00Z"
message_id = "<numbers@work.example>"
body = "Please review."

[[message]]
name = "standup"
folder = "INBOX"
from = "Team <team@work.example>"
subject = "Standup notes"
date = "2026-08-28T09:00:00Z"
message_id = "<standup@work.example>"
flags = ["\\Seen"]
body = "Nothing blocking."
"#;

const HOME: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"
[[folder]]
path = "Saved"
special_use = "archive"

[[message]]
name = "dinner"
folder = "INBOX"
from = "Mum <mum@home.example>"
to = ["peter@home.example"]
subject = "Dinner Sunday?"
date = "2026-08-31T18:00:00Z"
message_id = "<dinner@home.example>"
body = "Are you free?"

[[message]]
name = "receipt"
folder = "INBOX"
from = "Shop <shop@home.example>"
subject = "Your receipt"
date = "2026-08-29T12:00:00Z"
message_id = "<receipt@home.example>"
flags = ["\\Seen"]
body = "Thanks for your order."
"#;

struct World {
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
            .unwrap_or_else(|e| panic!("expected JSON, got {e}:\n{}\n{}", self.stdout, self.stderr))
    }
    fn error_code(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.stderr)
            .map(|v| v["error"]["code"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
    }
}

impl World {
    fn new() -> World {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-multi-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.work]\n\
             address = \"peter@work.example\"\n\
             imap_host = \"127.0.0.1\"\n\
             \n\
             [accounts.home]\n\
             address = \"peter@home.example\"\n\
             imap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");

        let w = World {
            state: dir.join("state"),
            config,
            dir,
        };
        w.seed("work", WORK);
        w.seed("home", HOME);
        w
    }

    fn seed(&self, account: &str, manifest: &str) {
        let path = self.dir.join(format!("{account}.toml"));
        std::fs::write(&path, manifest).expect("write fixture");
        let run = self.run(&[
            "--account",
            account,
            "debug",
            "fixture",
            "load",
            path.to_str().unwrap(),
        ]);
        assert_eq!(run.status, 0, "seeding {account} failed: {}", run.stderr);
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "multi")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("COLUMNS", "120")
            .env("NO_COLOR", "1")
            // Nothing here may put a popup on the desktop of
            // whoever ran the suite (§11).
            .env("MAILBOX_NOTIFIER", "none")
            .env_remove("MAILBOX_AGENT")
            .output()
            .expect("run the binary");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let run = self.run(&full);
        assert_eq!(
            run.status,
            0,
            "`mailbox {}` failed:\n{}\n{}",
            full.join(" "),
            run.stdout,
            run.stderr
        );
        run.json()
    }

    /// Subjects in result order — the shape most assertions want.
    fn subjects(&self, args: &[&str]) -> Vec<String> {
        self.json(args)["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["subject"].as_str().unwrap().to_string())
            .collect()
    }

    /// The stable id of a message, by the `Message-ID` its fixture pinned.
    fn id(&self, message_id: &str) -> String {
        for account in ["work", "home"] {
            let dump: serde_json::Value = serde_json::from_str(
                &self
                    .run(&["--account", account, "debug", "store-dump"])
                    .stdout,
            )
            .expect("dump");
            if let Some(found) = dump["messages"].as_array().and_then(|ms| {
                ms.iter()
                    .find(|m| m["message_id"].as_str() == Some(message_id))
            }) {
                return found["id"].as_str().unwrap().to_string();
            }
        }
        panic!("no message with Message-ID {message_id}");
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// §7: "one list". The default listing is both accounts, ordered as though they
/// were one mailbox.
#[test]
fn the_unified_inbox_merges_both_accounts() {
    let w = World::new();
    // Unread first, then newest first — across accounts, not within them.
    assert_eq!(
        w.subjects(&["ls"]),
        vec![
            "Dinner Sunday?",    // unread, 31 Aug, home
            "Quarterly numbers", // unread, 30 Aug, work
            "Your receipt",      // read,   29 Aug, home
            "Standup notes",     // read,   28 Aug, work
        ]
    );
}

#[test]
fn every_row_says_which_account_it_came_from() {
    let w = World::new();
    let rows = w.json(&["ls"]);
    let by_subject: std::collections::BTreeMap<&str, &str> = rows["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["subject"].as_str().unwrap(),
                m["account"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(by_subject["Quarterly numbers"], "work");
    assert_eq!(by_subject["Dinner Sunday?"], "home");
}

/// §7: "`account:` to split".
#[test]
fn the_account_term_splits_the_unified_view() {
    let w = World::new();
    assert_eq!(
        w.subjects(&["search", "account:home"]),
        vec!["Dinner Sunday?", "Your receipt"]
    );
    assert_eq!(
        w.subjects(&["search", "account:work"]),
        vec!["Quarterly numbers", "Standup notes"]
    );
    assert!(w.subjects(&["search", "account:nonexistent"]).is_empty());
}

#[test]
fn account_restricts_every_command() {
    let w = World::new();
    assert_eq!(
        w.subjects(&["--account", "work", "ls"]),
        vec!["Quarterly numbers", "Standup notes"]
    );

    let status = w.json(&["--account", "home", "status"]);
    assert_eq!(status["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(status["accounts"][0]["account"], "home");
}

/// Ordering and structured search must combine across accounts, not per
/// account and then concatenated.
#[test]
fn queries_apply_across_accounts() {
    let w = World::new();
    assert_eq!(
        w.subjects(&["search", "is:unread"]),
        vec!["Dinner Sunday?", "Quarterly numbers"]
    );
    assert_eq!(
        w.subjects(&["search", "before:2026-08-30"]),
        vec!["Your receipt", "Standup notes"]
    );
    // Full text reaches both stores' FTS indexes.
    assert_eq!(
        w.subjects(&["search", "\"Are you free\""]),
        vec!["Dinner Sunday?"]
    );
}

/// One cursor paginates the union. The sort key is total across accounts, so
/// "everything after this row" means the same thing in every store — which is
/// what makes a single opaque cursor correct rather than a lucky coincidence.
#[test]
fn one_cursor_paginates_the_union() {
    let w = World::new();
    let all = w.subjects(&["ls", "-n", "100"]);
    assert_eq!(all.len(), 4);

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args: Vec<String> = vec!["ls".into(), "-n".into(), "1".into()];
        if let Some(c) = &cursor {
            args.push("--cursor".into());
            args.push(c.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = w.json(&refs);
        for m in page["messages"].as_array().unwrap() {
            seen.push(m["subject"].as_str().unwrap().to_string());
        }
        match page["cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, all, "paging the union lost or reordered rows");
}

#[test]
fn paging_is_total_in_every_sort_order() {
    let w = World::new();
    for order in [vec!["ls"], vec!["ls", "--by-date"], vec!["ls", "--reverse"]] {
        let mut full = order.clone();
        full.extend_from_slice(&["-n", "100"]);
        let all = w.subjects(&full);

        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut args: Vec<String> = order.iter().map(|s| s.to_string()).collect();
            args.extend(["-n".to_string(), "2".to_string()]);
            if let Some(c) = &cursor {
                args.push("--cursor".into());
                args.push(c.clone());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let page = w.json(&refs);
            for m in page["messages"].as_array().unwrap() {
                seen.push(m["subject"].as_str().unwrap().to_string());
            }
            match page["cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        assert_eq!(seen, all, "{order:?} lost or reordered rows");
    }
}

/// §5.2 hashes the account into every message id, so an id names an account as
/// well as a message and routing is a lookup rather than a guess.
#[test]
fn show_routes_to_the_account_that_owns_the_message() {
    let w = World::new();
    let shown = w.json(&["show", &w.id("dinner@home.example")]);
    assert_eq!(shown["message"]["account"], "home");
    assert_eq!(shown["message"]["subject"], "Dinner Sunday?");

    let shown = w.json(&["show", &w.id("numbers@work.example")]);
    assert_eq!(shown["message"]["account"], "work");
}

#[test]
fn a_write_reaches_the_right_account() {
    let w = World::new();
    let id = w.id("dinner@home.example");
    w.json(&["flag", &id, "+flagged"]);

    let shown = w.json(&["show", &id]);
    let flags: Vec<&str> = shown["message"]["flags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(flags.contains(&"flagged"), "{flags:?}");

    // Exactly one account queued the write.
    let status = w.json(&["status"]);
    let queued: u64 = status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["queued_ops"].as_u64().unwrap())
        .sum();
    assert_eq!(queued, 1);
    let home = status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["account"] == "home")
        .unwrap();
    assert_eq!(home["queued_ops"], 1);
}

/// One `flag` naming messages in both accounts is one command and two
/// transactions, and every message ends up changed.
#[test]
fn a_write_can_span_accounts() {
    let w = World::new();
    let work = w.id("numbers@work.example");
    let home = w.id("dinner@home.example");

    let out = w.json(&["flag", &work, &home, "+seen"]);
    assert_eq!(out["changed"], 2);

    for id in [&work, &home] {
        let shown = w.json(&["show", id]);
        let flags: Vec<&str> = shown["message"]["flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert!(flags.contains(&"seen"), "{id}: {flags:?}");
    }
}

/// `archive` is a different folder path on different servers. In a unified
/// mailbox it has to be resolved per account, not once.
#[test]
fn the_move_sugar_resolves_per_account() {
    let w = World::new();
    let work = w.id("numbers@work.example");
    let home = w.id("dinner@home.example");

    w.json(&["archive", &work, &home]);

    // The work account calls it `Archive`; the home account calls it `Saved`.
    assert_eq!(w.json(&["show", &work])["message"]["folder"], "Archive");
    assert_eq!(w.json(&["show", &home])["message"]["folder"], "Saved");
}

/// An id prefix that matches in two accounts is ambiguous, and saying so is the
/// point: the caller most hurt by a silent pick is an agent (§5.2).
#[test]
fn an_id_prefix_matching_two_accounts_is_ambiguous() {
    let w = World::new();
    let work = w.id("numbers@work.example");
    let home = w.id("dinner@home.example");

    // Find the shortest prefix length at which these two ids still differ, then
    // step back one — that prefix matches both.
    let common = work
        .chars()
        .zip(home.chars())
        .take_while(|(a, b)| a == b)
        .count();
    if common == 0 {
        // The ids diverge immediately, so no shared prefix exists to be
        // ambiguous about. Nothing to assert, and forcing it would be a test
        // about hash values rather than about routing.
        return;
    }

    let run = w.run(&["--json", "show", &work[..common]]);
    assert_eq!(run.status, 3, "{}", run.stderr);
    assert_eq!(run.error_code(), "ambiguous");
}

#[test]
fn folders_and_status_cover_every_account() {
    let w = World::new();
    let folders = w.json(&["folders"]);
    let accounts: std::collections::BTreeSet<&str> = folders["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["account"].as_str().unwrap())
        .collect();
    assert_eq!(
        accounts,
        ["home", "work"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
    );

    let status = w.json(&["status"]);
    assert_eq!(status["accounts"].as_array().unwrap().len(), 2);
    let total: u64 = status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["messages"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 4);
}

/// A unified listing and a per-account one are different listings, so an index
/// into one must not resolve against the other (§5.2).
#[test]
fn listing_history_is_scoped() {
    let w = World::new();

    // The unified listing's first row is the newest unread: the home message.
    w.json(&["ls"]);
    assert_eq!(w.json(&["show", "1"])["message"]["account"], "home");

    // The work-only listing's first row is a different message.
    w.json(&["--account", "work", "ls"]);
    assert_eq!(
        w.json(&["--account", "work", "show", "1"])["message"]["account"],
        "work"
    );

    // And the unified index is untouched by the account-scoped listing.
    w.json(&["ls"]);
    assert_eq!(w.json(&["show", "1"])["message"]["account"], "home");
}

/// §12's `unified_inbox = false` makes every command act on one account.
#[test]
fn unified_inbox_can_be_turned_off() {
    let w = World::new();
    std::fs::write(
        &w.config,
        format!(
            "[general]\nunified_inbox = false\n\n{}",
            std::fs::read_to_string(&w.config).unwrap()
        ),
    )
    .unwrap();

    let status = w.json(&["status"]);
    assert_eq!(
        status["accounts"].as_array().unwrap().len(),
        1,
        "with the unified inbox off, one account is opened"
    );
    // `--account` still reaches the other one.
    assert_eq!(
        w.subjects(&["--account", "work", "ls"]),
        vec!["Quarterly numbers", "Standup notes"]
    );
}

/// `send --from` picks the account that owns the identity, which is what makes
/// it do the obvious thing in a unified mailbox (§7).
#[test]
fn send_from_chooses_the_account_by_identity() {
    let w = World::new();
    let result = w.json(&[
        "--dry-run",
        "send",
        "--from",
        "peter@home.example",
        "--to",
        "mum@home.example",
        "--subject",
        "Yes",
        "--body",
        "See you Sunday.",
    ]);
    let raw = result["raw"].as_str().unwrap();
    assert!(raw.contains("From: peter@home.example"), "{raw}");
    // The Message-ID domain comes from the sender, so it names the right
    // account's domain too.
    assert!(
        raw.lines()
            .any(|l| l.starts_with("Message-ID: ") && l.contains("@home.example>")),
        "{raw}"
    );
}

/// `debug store-dump` stays a stable golden-file oracle with several accounts
/// open: keyed by account, ordered by it (§14.3).
#[test]
fn the_store_dump_is_keyed_by_account() {
    let w = World::new();
    let run = w.run(&["debug", "store-dump"]);
    let dump: serde_json::Value = serde_json::from_str(&run.stdout).expect("dump");

    assert!(dump["work"]["messages"].is_array());
    assert!(dump["home"]["messages"].is_array());
    assert_eq!(dump["work"]["messages"].as_array().unwrap().len(), 2);

    // Stable across runs, which is what makes it an oracle at all.
    let again = w.run(&["debug", "store-dump"]);
    assert_eq!(run.stdout, again.stdout);
}

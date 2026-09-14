//! L0: the mobile facade talks to the same store the CLI does, with no server.

use mailbox_core::clock::Clock;
use mailbox_core::fixture::Fixture;
use mailbox_core::store::Store;
use mailbox_core::Tracer;
use mailbox_ffi::Mailbox;
use serde_json::Value;

const FIXTURE: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[folder]]
path = "Archive"
special_use = "archive"

[[folder]]
path = "Trash"
special_use = "trash"

[[message]]
name = "root"
folder = "INBOX"
from = "Alice <alice@example.com>"
to = ["peter@example.com"]
subject = "Invoice 42"
date = "2026-08-01T10:00:00Z"
message_id = "<inv42@example.com>"
body = "Please pay the invoice."

[[message]]
name = "reply"
folder = "INBOX"
from = "Peter <peter@example.com>"
to = ["alice@example.com"]
subject = "Re: Invoice 42"
date = "2026-08-01T12:00:00Z"
message_id = "<reply@example.com>"
in_reply_to = "<inv42@example.com>"
flags = ["\\Seen"]
body = "Paid, thanks."
"#;

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("mailbox-ffi-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn seeded() -> (TempDir, Mailbox, String) {
    let tmp = TempDir::new();
    let config_path = tmp.path().join("config.toml");
    let state = tmp.path().join("state");
    std::fs::write(
        &config_path,
        r#"
[accounts.test]
address = "peter@example.com"
imap_host = "127.0.0.1"
imap_port = 143
imap_security = "none"
require_tls = false
"#,
    )
    .unwrap();

    let mut store = Store::open(
        state.join("test"),
        "test",
        Tracer::disabled(),
        Clock::fixed(0),
    )
    .expect("open store");
    let fixture = Fixture::parse(FIXTURE).expect("parse fixture");
    let ids = fixture.seed_store(&mut store).expect("seed");
    drop(store);

    let mb = Mailbox::open(&config_path, &state).expect("open mailbox");
    let root_id = ids.get("root").expect("root").clone();
    (tmp, mb, root_id)
}

fn payload(json: &str) -> Value {
    serde_json::from_str(json).unwrap_or_else(|e| panic!("not json ({e}): {json}"))
}

#[test]
fn empty_mailbox_lists_nothing() {
    let tmp = TempDir::new();
    let config = tmp.path().join("config.toml");
    let state = tmp.path().join("state");
    let mb = Mailbox::open(&config, &state).expect("open");
    let out = mb.exec("ls", "{}").expect("ls");
    let v = payload(&out);
    assert_eq!(v["schema"], "mailbox/v1");
    assert_eq!(v["command"], "ls");
    assert_eq!(v["messages"].as_array().unwrap().len(), 0);
}

#[test]
fn list_returns_inbox_unread_first() {
    let (_tmp, mb, _id) = seeded();
    let out = mb.exec("ls", r#"{"limit": 50}"#).expect("ls");
    let v = payload(&out);
    let messages = v["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2);
    // Unread first: Invoice 42 is unseen, the reply is seen.
    assert_eq!(messages[0]["subject"], "Invoice 42");
    assert!(messages[0]["from"][0]["address"]
        .as_str()
        .unwrap()
        .contains("alice"));
}

#[test]
fn search_matches_a_prefix() {
    let (_tmp, mb, _id) = seeded();
    let out = mb
        .exec("search", r#"{"query": "from:alice"}"#)
        .expect("search");
    let v = payload(&out);
    let messages = v["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["subject"], "Invoice 42");
}

#[test]
fn show_returns_the_body() {
    let (_tmp, mb, id) = seeded();
    let out = mb
        .exec("show", &format!(r#"{{"id":"{id}"}}"#))
        .expect("show");
    let v = payload(&out);
    assert!(v["message"]["body"]["content"]
        .as_str()
        .unwrap()
        .contains("Please pay"));
}

#[test]
fn flag_and_archive_are_instant() {
    let (_tmp, mb, id) = seeded();
    mb.exec(
        "flag",
        &format!(r#"{{"ids":["{id}"],"changes":["+seen"]}}"#),
    )
    .expect("flag");
    mb.exec("archive", &format!(r#"{{"ids":["{id}"]}}"#))
        .expect("archive");
    let out = mb.exec("ls", "{}").expect("ls");
    let v = payload(&out);
    // Archived out of the inbox.
    let subjects: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["subject"].as_str().unwrap())
        .collect();
    assert!(!subjects.contains(&"Invoice 42"));
}

#[test]
fn hosted_dispatch_covers_the_shared_web_clients_missing_calls() {
    let (_tmp, mb, id) = seeded();

    let identities = payload(&mb.exec("identities", "{}").unwrap());
    assert_eq!(identities["identities"][0]["address"], "peter@example.com");

    let moved = payload(
        &mb.exec("move", &format!(r#"{{"ids":["{id}"],"folder":"Archive"}}"#))
            .unwrap(),
    );
    assert_eq!(moved["changed"], 1);

    let snoozed = payload(
        &mb.exec(
            "snooze",
            &format!(r#"{{"ids":["{id}"],"until":"2030-01-01"}}"#),
        )
        .unwrap(),
    );
    assert_eq!(snoozed["snoozed"][0], id);
    let listing = payload(&mb.exec("snoozed", "{}").unwrap());
    assert_eq!(listing["snoozed"][0]["id"], id);
    let woke = payload(
        &mb.exec("snooze", &format!(r#"{{"ids":["{id}"]}}"#))
            .unwrap(),
    );
    assert_eq!(woke["woke"][0], id);

    let outbox = payload(&mb.exec("outbox", "{}").unwrap());
    assert_eq!(outbox["messages"].as_array().unwrap().len(), 0);
    assert!(mb
        .database_snapshot()
        .unwrap()
        .starts_with(b"SQLite format 3\0"));
}

#[test]
fn add_account_fills_gmail_hosts_and_keeps_the_password_out_of_config() {
    let tmp = TempDir::new();
    let config = tmp.path().join("config.toml");
    let state = tmp.path().join("state");
    let mb = Mailbox::open(&config, &state).expect("open");
    let out = mb
        .exec(
            "add_account",
            r#"{"address":"peter@gmail.com","password":"secret"}"#,
        )
        .expect("add");
    let v = payload(&out);
    assert_eq!(v["id"], "peter");

    let text = std::fs::read_to_string(&config).expect("read config");
    assert!(text.contains("imap.gmail.com"), "{text}");
    assert!(
        !text.contains("secret"),
        "the password must not land in config.toml: {text}"
    );

    let accounts = payload(&mb.exec("accounts", "{}").expect("accounts"));
    assert_eq!(accounts["accounts"][0]["has_credential"], true);
}

#[test]
fn providers_knows_gmail_and_not_example() {
    let tmp = TempDir::new();
    let mb = Mailbox::open(tmp.path().join("c.toml"), tmp.path().join("s")).expect("open");
    let gmail = payload(
        &mb.exec("providers", r#"{"address":"a@gmail.com"}"#)
            .unwrap(),
    );
    assert_eq!(gmail["known"], true);
    let other = payload(
        &mb.exec("providers", r#"{"address":"a@example.com"}"#)
            .unwrap(),
    );
    assert_eq!(other["known"], false);
}

#[test]
fn unknown_method_is_a_structured_error() {
    let tmp = TempDir::new();
    let mb = Mailbox::open(tmp.path().join("c.toml"), tmp.path().join("s")).expect("open");
    let err = mb.exec("nope", "{}").unwrap_err();
    assert_eq!(err.error.code, mailbox_api::ErrorCode::Usage);
}

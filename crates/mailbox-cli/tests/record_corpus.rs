//! A helper, not a test: records the committed L1 corpus.
//!
//! Run with `MAILBOX_RECORD_CORPUS=1 cargo test -p mailbox-cli --test
//! record_corpus -- --ignored --nocapture`. Ignored by default so an ordinary
//! run does not rewrite the corpus underneath the suite that gates on it.

use std::path::PathBuf;
use std::process::Command;

#[test]
#[ignore = "rewrites the committed L1 corpus; run deliberately"]
fn record_the_l1_corpus() {
    let server = mailbox_l3::ImapServer::start(vec![
        mailbox_l3::Message::simple(1, "First", "one@example.com"),
        mailbox_l3::Message::simple(2, "Second", "two@example.com"),
        mailbox_l3::Message::simple(3, "Third", "three@example.com"),
    ])
    .expect("server");

    let dir = std::env::temp_dir().join("mailbox-record-corpus");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n\
             imap_port = {}\nimap_security = \"none\"\nrequire_tls = false\n",
            server.port()
        ),
    )
    .expect("config");

    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transcripts/l3-imap-first-sync.ndjson");

    let status = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(["--json", "sync", "--once"])
        .env("MAILBOX_CONFIG", &config)
        .env("MAILBOX_STATE_DIR", dir.join("state"))
        // Nothing here may put a popup on the desktop of
        // whoever ran the suite (§11).
        .env("MAILBOX_NOTIFIER", "none")
        .env("MAILBOX_PASSWORD_TEST", "testpass")
        .env("MAILBOX_CLOCK", "2026-09-02T12:00:00Z")
        .env("MAILBOX_SEED", "l1")
        .env(
            "MAILBOX_RECORD",
            "a first sync of three messages, no CONDSTORE",
        )
        .env("MAILBOX_RECORD_PATH", &out)
        .status()
        .expect("run");
    assert!(status.success());
    println!("recorded {}", out.display());
}

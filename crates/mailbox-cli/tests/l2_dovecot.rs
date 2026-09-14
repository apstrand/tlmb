//! L2 (§14.2): the sync engine against a real Dovecot, seeded from fixtures.
//!
//! M1's verification requirement is L0 plus this. What L2 catches that L0
//! cannot is everything about the *conversation*: whether `QRESYNC` was really
//! used, whether a `UIDVALIDITY` change is recovered from, whether an expunge
//! another client performed reaches us, whether a modified-UTF-7 folder name
//! survives a round trip.
//!
//! Every test drives the CLI and asserts on `--json`, so this suite is also the
//! largest consumer of the agent contract of §6.4 — a breakage of it fails a
//! build rather than a user.

mod l2;

use l2::{server_available, World, PORT};

/// Every test starts with this. It brings the shared container up if needed and
/// returns false when Docker is unavailable, which is a loud skip on a laptop
/// and a hard failure in CI (`MAILBOX_L2_REQUIRED=1`).
macro_rules! l2_or_skip {
    () => {
        if !server_available() {
            return;
        }
    };
}

const BASIC: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[folder]]
path = "Archive"
special_use = "archive"

[[message]]
name = "root"
folder = "INBOX"
from = "Alice <alice@example.com>"
to = ["peter@example.com"]
subject = "Invoice 42"
date = "2026-08-01T10:00:00Z"
message_id = "<inv42@example.com>"
body = "Please pay the invoice."
[[message.attachment]]
filename = "invoice.pdf"
mime_type = "application/pdf"
text = "%PDF-1.4 not really a pdf"

[[message]]
name = "reply"
folder = "INBOX"
from = "Peter <peter@example.com>"
to = ["alice@example.com"]
subject = "Re: Invoice 42"
date = "2026-08-01T12:00:00Z"
message_id = "<reply@example.com>"
in_reply_to = "<inv42@example.com>"
references = ["<inv42@example.com>"]
flags = ["\\Seen"]
body = "Paid, thanks."

[[message]]
name = "digest"
folder = "Archive"
from = "News <news@example.org>"
subject = "Weekly digest"
date = "2026-08-05T08:00:00Z"
message_id = "<digest@example.org>"
flags = ["\\Seen", "\\Flagged"]
body = "Nothing to see here."
"#;

/// Dated well outside the 30-day prefetch window of §5.1, so its body must
/// *not* arrive on an ordinary pass.
const ANCIENT: &str = r#"
[[message]]
name = "ancient"
folder = "Archive"
from = "Archivist <archivist@example.org>"
subject = "From the vault"
date = "2024-01-01T00:00:00Z"
message_id = "<ancient@example.org>"
body = "A very old message."
"#;

const EXTRA: &str = r#"
[[message]]
name = "later"
folder = "INBOX"
from = "Carol <carol@example.com>"
subject = "Something new"
date = "2026-08-15T09:00:00Z"
message_id = "<later@example.com>"
body = "Arrived after the first sync."

[[message]]
name = "second-reply"
folder = "INBOX"
from = "Alice <alice@example.com>"
subject = "Re: Invoice 42"
date = "2026-08-02T09:00:00Z"
message_id = "<reply2@example.com>"
in_reply_to = "<reply@example.com>"
references = ["<inv42@example.com>", "<reply@example.com>"]
body = "You are welcome."
"#;

#[test]
fn a_first_sync_brings_down_folders_and_messages() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);

    let result = w.sync();
    let report = &result["synced"][0];
    assert_eq!(report["messages_added"], 3);
    assert!(
        report["folders_examined"].as_u64().unwrap() >= 6,
        "Dovecot auto-creates five folders plus INBOX"
    );

    // Every fixture message is present, found by its Message-ID rather than by
    // anything the server assigned.
    for mid in [
        "inv42@example.com",
        "reply@example.com",
        "digest@example.org",
    ] {
        assert!(w.has(mid), "{mid} should have synced");
    }
}

#[test]
fn special_use_folders_are_recognized() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let folders = w.json(&["folders"]);
    let by_use: std::collections::BTreeMap<String, String> = folders["folders"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| {
            Some((
                f["special_use"].as_str()?.to_string(),
                f["path"].as_str()?.to_string(),
            ))
        })
        .collect();

    // From SPECIAL-USE, which Dovecot advertises. The name heuristic would get
    // these too, so the assertion that matters is that the paths are right.
    assert_eq!(by_use.get("inbox").map(String::as_str), Some("INBOX"));
    assert_eq!(by_use.get("sent").map(String::as_str), Some("Sent"));
    assert_eq!(by_use.get("archive").map(String::as_str), Some("Archive"));
    assert_eq!(by_use.get("junk").map(String::as_str), Some("Junk"));
}

#[test]
fn counts_match_what_the_server_holds() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let folders = w.json(&["folders"]);
    let inbox = folders["folders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "INBOX")
        .expect("an INBOX");
    assert_eq!(inbox["total"], 2);
    assert_eq!(inbox["unread"], 1, "the reply was appended with \\Seen");

    let status = w.json(&["status"]);
    assert_eq!(status["accounts"][0]["messages"], 3);
    assert_eq!(status["accounts"][0]["unread"], 1);
}

#[test]
fn threading_survives_a_real_round_trip() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let dump = w.dump(None);
    let threads = dump["threads"].as_object().expect("threads");
    let root_id = w.id("inv42@example.com");
    let reply_id = w.id("reply@example.com");

    let members = threads
        .get(&root_id)
        .expect("the invoice thread")
        .as_array()
        .unwrap();
    assert_eq!(members.len(), 2);
    assert!(members
        .iter()
        .any(|m| m.as_str() == Some(reply_id.as_str())));

    // And the digest is its own conversation.
    assert!(threads.contains_key(&w.id("digest@example.org")));
}

#[test]
fn bodies_and_attachments_arrive() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let shown = w.json(&["show", &w.id("inv42@example.com")]);
    let message = &shown["message"];
    assert_eq!(message["subject"], "Invoice 42");
    assert!(message["body"]["content"]
        .as_str()
        .unwrap()
        .contains("Please pay the invoice."));
    assert_eq!(message["has_attachment"], true);

    let attachments = message["attachments"].as_array().unwrap();
    let pdf = attachments
        .iter()
        .find(|a| a["filename"] == "invoice.pdf")
        .expect("the pdf");
    assert_eq!(pdf["mime_type"], "application/pdf");
    // IMAP part numbering, which `attach <id> <part>` will take in M2.
    assert_eq!(pdf["part"], "2");
}

#[test]
fn search_works_over_synced_mail() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let subjects = |v: &serde_json::Value| -> Vec<String> {
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["subject"].as_str().unwrap().to_string())
            .collect()
    };

    assert_eq!(
        subjects(&w.json(&["search", "from:alice"])),
        vec!["Invoice 42"]
    );
    assert_eq!(
        subjects(&w.json(&["search", "is:flagged"])),
        vec!["Weekly digest"]
    );
    assert_eq!(
        subjects(&w.json(&["search", "has:attachment"])),
        vec!["Invoice 42"]
    );
    assert_eq!(
        subjects(&w.json(&["search", "in:archive"])),
        vec!["Weekly digest"]
    );
    // Full text, from a body that only exists because the body fetch worked.
    assert_eq!(
        subjects(&w.json(&["search", "\"Nothing to see here\""])),
        vec!["Weekly digest"]
    );
}

/// The second pass must be a delta, not a rescan — and must not re-add what it
/// already has.
#[test]
fn an_unchanged_second_sync_is_a_no_op() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();
    let before = w.dump(None);

    let result = w.sync();
    assert_eq!(result["synced"][0]["messages_added"], 0);
    assert_eq!(result["synced"][0]["messages_removed"], 0);

    // And nothing about the store changed. This is the property that makes a
    // crashed pass safe to replay (§5.1's state.toml ordering).
    assert_eq!(w.dump(None), before);
}

#[test]
fn new_mail_arrives_on_the_next_pass() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    w.append("later", "INBOX", EXTRA);
    let result = w.sync();
    assert_eq!(result["synced"][0]["messages_added"], 1);
    assert!(w.has("later@example.com"));
}

/// A reply that arrives later must join the conversation it belongs to — the
/// case a scoped thread rebuild would get wrong.
#[test]
fn a_late_reply_joins_its_thread() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    w.append("second-reply", "INBOX", EXTRA);
    w.sync();

    let dump = w.dump(None);
    let root_id = w.id("inv42@example.com");
    let members = dump["threads"][&root_id]
        .as_array()
        .expect("the invoice thread");
    assert_eq!(
        members.len(),
        3,
        "the late reply should have joined: {members:?}"
    );
    assert!(members
        .iter()
        .any(|m| m.as_str() == Some(w.id("reply2@example.com").as_str())));
}

/// A flag another client set must reach us. This is the `CONDSTORE`/`QRESYNC`
/// delta path doing its job.
#[test]
fn a_flag_changed_on_the_server_propagates() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let id = w.id("inv42@example.com");
    let before = w.json(&["show", &id]);
    assert!(!before["message"]["flags"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f == "seen"));

    let uid = *w.uids.get("root").expect("the seeder recorded a UID");
    w.set_server_flags("INBOX", uid, "\\Seen \\Flagged");

    let result = w.sync();
    assert!(result["synced"][0]["messages_updated"].as_u64().unwrap() >= 1);

    let after = w.json(&["show", &id]);
    let flags: Vec<String> = after["message"]["flags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap().to_string())
        .collect();
    assert!(flags.contains(&"seen".to_string()), "{flags:?}");
    assert!(flags.contains(&"flagged".to_string()), "{flags:?}");
}

/// An expunge another client performed must remove the message here too.
#[test]
fn an_expunge_propagates() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();
    assert!(w.has("reply@example.com"));

    let uid = *w.uids.get("reply").expect("a recorded UID");
    w.expunge("INBOX", uid);

    let result = w.sync();
    assert_eq!(result["synced"][0]["messages_removed"], 1);
    assert!(
        !w.has("reply@example.com"),
        "the expunged message should be gone"
    );
    // The rest of the mailbox is untouched.
    assert!(w.has("inv42@example.com"));
}

/// §5.3: a `UIDVALIDITY` change invalidates every UID we hold, and the folder is
/// rescanned. The messages survive it, because their ids come from their
/// content rather than from the UID (§5.2).
#[test]
fn a_uidvalidity_change_forces_a_rescan_without_losing_identity() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();
    let digest_id = w.id("digest@example.org");

    // Delete and recreate the folder, which is how a real UIDVALIDITY change
    // happens, then put the message back.
    w.recreate_folder("Archive");
    w.append("digest", "Archive", BASIC);

    let result = w.sync();
    let resets = result["synced"][0]["uidvalidity_resets"]
        .as_array()
        .unwrap();
    assert!(
        resets.iter().any(|f| f == "Archive"),
        "the rescan should have been reported: {resets:?}"
    );

    assert!(w.has("digest@example.org"), "the message should come back");
    assert_eq!(
        w.id("digest@example.org"),
        digest_id,
        "and with the same stable id — that is the whole point of §5.2"
    );
}

/// A folder deleted on the server stops being synced, and its messages go with
/// it when no other folder places them.
#[test]
fn a_deleted_folder_disappears() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    // A folder the server's own config does not auto-create, so `DELETE`
    // actually removes it. Deleting `Archive` here would test Dovecot's
    // `auto = subscribe`, not our sync engine.
    w.append(
        "digest",
        "Project",
        &BASIC.replace("<digest@example.org>", "<project@example.org>"),
    );
    w.sync();
    assert!(w.has("project@example.org"));

    w.delete_folder("Project");

    let result = w.sync();
    assert_eq!(result["synced"][0]["messages_removed"], 1);
    assert!(!w.has("project@example.org"));

    let folders = w.json(&["folders"]);
    assert!(
        !folders["folders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "Project"),
        "the folder itself should be gone too"
    );
}

/// Non-ASCII folder names go out as modified UTF-7 and come back decoded
/// (RFC 3501 §5.1.3). Getting this wrong shows up as `&U,BTFw-` in the UI.
#[test]
fn unicode_folder_names_round_trip() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.append(
        "digest",
        "Ärchive/日本語",
        &BASIC.replace("<digest@example.org>", "<unicode@example.org>"),
    );

    w.sync();
    let folders = w.json(&["folders"]);
    let paths: Vec<String> = folders["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.iter().any(|p| p == "Ärchive/日本語"), "{paths:?}");

    // And the message inside it is reachable by a query that names the folder.
    let hits = w.json(&["search", "in:日本語"]);
    assert_eq!(hits["messages"].as_array().unwrap().len(), 1);
}

/// §6.7's `--trace`: the file must carry the whole conversation, with the
/// password redacted, and `--until` must stop the pass where it says.
#[test]
fn the_trace_records_the_conversation_without_the_password() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);

    let trace = w.dir.join("sync.trace.ndjson");
    let out = w.run(&[
        "sync",
        "--once",
        "--json",
        "--trace",
        trace.to_str().unwrap(),
    ]);
    assert_eq!(out.status, 0, "{}", out.stderr);

    let text = std::fs::read_to_string(&trace).expect("a trace file");
    assert!(
        !text.contains("testpass"),
        "the password must never reach a trace"
    );

    let events: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("NDJSON"))
        .collect();
    let names: Vec<&str> = events.iter().filter_map(|e| e["event"].as_str()).collect();
    for expected in [
        "imap.connect",
        "imap.send",
        "imap.recv",
        "sync.begin",
        "sync.end",
        "store.txn.commit",
    ] {
        assert!(
            names.contains(&expected),
            "the trace should contain {expected}: {names:?}"
        );
    }

    // Monotonic sequence numbers, which is what lets a test assert on order
    // instead of on wall-clock behaviour.
    let seqs: Vec<u64> = events.iter().filter_map(|e| e["seq"].as_u64()).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "sequence numbers must be monotonic"
    );
}

#[test]
fn sync_until_stops_at_the_named_event() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);

    let trace = w.dir.join("until.trace.ndjson");
    let out = w.run(&[
        "sync",
        "--once",
        "--json",
        "--trace",
        trace.to_str().unwrap(),
        "--until",
        "sync.folder.end",
    ]);
    assert_eq!(out.status, 0, "{}", out.stderr);

    let result: serde_json::Value = serde_json::from_str(&out.stdout).expect("JSON");
    let examined = result["synced"][0]["folders_examined"].as_u64().unwrap();
    assert_eq!(
        examined, 1,
        "the pass should have stopped after the first folder"
    );
}

/// §6.4: `--dry-run` exits 0 without touching the network. Pointed at a port
/// nothing is listening on, it must still succeed.
#[test]
fn dry_run_touches_no_network() {
    l2_or_skip!();
    let w = World::new();
    std::fs::write(
        &w.config,
        format!(
            "[accounts.test]\naddress = \"{}\"\nimap_host = \"127.0.0.1\"\nimap_port = 1\nimap_security = \"none\"\nrequire_tls = false\n",
            w.user
        ),
    )
    .unwrap();

    let result = w.json(&["sync", "--dry-run"]);
    assert_eq!(result["would_sync"], true);
}

/// A wrong password must produce `auth_failed` and exit 4 (§6.6) — not a
/// generic failure an agent cannot branch on.
#[test]
fn a_bad_credential_is_auth_failed() {
    l2_or_skip!();
    let w = World::new();
    let (code, status) = run_with_password(&w, &["sync", "--once", "--json"], "wrong-password");
    assert_eq!(status, 4, "§6.6 gives authentication its own exit code");
    assert_eq!(code, "auth_failed");
}

/// Runs the CLI with a chosen password, returning `(error code, exit status)`.
fn run_with_password(w: &World, args: &[&str], password: &str) -> (String, i32) {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(args)
        .env("MAILBOX_CONFIG", &w.config)
        .env("MAILBOX_STATE_DIR", &w.state)
        // Nothing here may put a popup on the desktop of
        // whoever ran the suite (§11).
        .env("MAILBOX_NOTIFIER", "none")
        .env("MAILBOX_PASSWORD_TEST", password)
        .env("XDG_RUNTIME_DIR", &w.dir)
        .output()
        .expect("run the binary");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let v: serde_json::Value = serde_json::from_str(&stderr).unwrap_or(serde_json::Value::Null);
    (
        v["error"]["code"].as_str().unwrap_or_default().to_string(),
        output.status.code().unwrap_or(-1),
    )
}

/// §6.1: `folders --remote` answers "what is on the server, and how big is it"
/// without syncing.
///
/// The properties that make it worth having are that it needs no store and
/// creates none — so it works on an account that has never synced, which is
/// exactly when the question gets asked.
#[test]
fn remote_folders_lists_the_server_without_syncing() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);

    let remote = w.json(&["folders", "--remote"]);
    let folders = remote["folders"].as_array().expect("folders");

    let inbox = folders
        .iter()
        .find(|f| f["path"] == "INBOX")
        .expect("an INBOX");
    assert_eq!(
        inbox["total"], 2,
        "counts come from STATUS, not from a sync"
    );
    assert_eq!(inbox["unread"], 1);
    assert_eq!(inbox["special_use"], "inbox");

    let archive = folders
        .iter()
        .find(|f| f["path"] == "Archive")
        .expect("an Archive");
    assert_eq!(archive["total"], 1);

    // Inbox first, then the other special-use folders — the same order the
    // local listing uses, so the two views of one mailbox agree.
    assert_eq!(folders[0]["path"], "INBOX");

    // Nothing was written: the store still does not exist, and the local
    // listing is still empty.
    assert!(
        !w.state.join("test/meta.db").exists(),
        "a remote listing must not create a store"
    );
    let local = w.json(&["folders"]);
    assert!(local["folders"].as_array().unwrap().is_empty());
}

/// And after a sync the two agree about what is there.
#[test]
fn remote_and_local_folder_listings_agree_after_a_sync() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let names = |v: &serde_json::Value| -> Vec<(String, u64, u64)> {
        v["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["path"].as_str().unwrap().to_string(),
                    f["total"].as_u64().unwrap(),
                    f["unread"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(
        names(&w.json(&["folders", "--remote"])),
        names(&w.json(&["folders"]))
    );
}

/// §9/§12: plenty of providers separate the login name from the address —
/// a shared mailbox, a `domain\user` login, an account whose address is an
/// alias. `imap_user` is what the server is told; `address` is what the mail
/// says.
///
/// The seeded mailbox belongs to the *login*, so this only passes if the login
/// name reached the server. Sending the address instead would authenticate
/// fine against a server that takes any username and then sync somebody else's
/// empty mailbox — which is exactly the failure that looks like "sync is
/// broken" and is not.
#[test]
fn a_login_name_may_differ_from_the_address() {
    l2_or_skip!();
    let mut w = World::with_separate_login();
    assert_ne!(w.user, w.login);
    w.seed(BASIC);

    let result = w.sync();
    assert_eq!(result["synced"][0]["messages_added"], 3);
    assert!(w.has("inv42@example.com"));

    // And the contract says which name was used, so "wrong username" is
    // distinguishable from "wrong password" without reading a trace.
    let accounts = w.json(&["accounts"]);
    assert_eq!(accounts["accounts"][0]["login_user"], w.login);
    assert_eq!(accounts["accounts"][0]["address"], w.user);

    let tested = w.json(&["accounts", "test"]);
    assert_eq!(tested["login_user"], w.login);
    assert_eq!(tested["authenticated"], true);
}

/// Without `imap_user`, the address is the login. The default has to keep
/// working, or every ordinary account breaks.
#[test]
fn the_address_is_the_login_by_default() {
    l2_or_skip!();
    let mut w = World::new();
    assert_eq!(w.user, w.login);
    w.seed(BASIC);
    w.sync();

    let accounts = w.json(&["accounts"]);
    assert_eq!(accounts["accounts"][0]["login_user"], w.user);
}

/// `accounts test` reports which of §5.3's fast paths this server offers.
#[test]
fn accounts_test_reports_the_sync_path() {
    l2_or_skip!();
    let w = World::new();
    let result = w.json(&["accounts", "test"]);
    assert_eq!(result["authenticated"], true);
    assert_eq!(result["sync_path"], "qresync", "Dovecot advertises QRESYNC");
    let caps: Vec<&str> = result["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for expected in [
        "CONDSTORE",
        "QRESYNC",
        "UIDPLUS",
        "SPECIAL-USE",
        "LIST-EXTENDED",
    ] {
        assert!(
            caps.contains(&expected),
            "Dovecot should advertise {expected}: {caps:?}"
        );
    }
}

/// §5.1: headers and structure for everything, bodies for the prefetch window.
/// Old mail keeps its body on the server until something asks for it.
#[test]
fn old_mail_is_fetched_lazily_and_sync_full_gets_it() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(ANCIENT);
    w.sync();

    assert!(
        w.has("ancient@example.org"),
        "the headers should arrive regardless"
    );
    // Normal `show` fetches a missing body on demand (§5.1). Inspect the
    // post-sync cache without making that explicit read request.
    let shown = w.json(&["show", "--offline", &w.id("ancient@example.org")]);
    assert_eq!(shown["message"]["subject"], "From the vault");
    assert!(
        shown["message"]["body"].is_null(),
        "a body outside the prefetch window must not have been downloaded"
    );

    // `sync --full` fetches everything, for offline use.
    let result = w.sync_with(&["--full"]);
    assert!(result["synced"][0]["bodies_fetched"].as_u64().unwrap() >= 1);

    let shown = w.json(&["show", &w.id("ancient@example.org")]);
    assert!(shown["message"]["body"]["content"]
        .as_str()
        .unwrap()
        .contains("A very old message."));
}

/// The convergence oracle in its M1 form (§14.3): two stores synced from the
/// same server state must be byte-identical.
#[test]
fn two_stores_of_the_same_mailbox_converge() {
    l2_or_skip!();
    let mut a = World::new();
    a.seed(BASIC);
    a.sync();

    // A second store for the same account, in its own directory.
    let mut b = World::new();
    b.user = a.user.clone();
    std::fs::write(&b.config, std::fs::read_to_string(&a.config).unwrap()).unwrap();
    b.sync();

    let (mut da, mut db) = (a.dump(None), b.dump(None));
    // The account id is the same; the store paths are not, and nothing in the
    // dump should mention them.
    for d in [&mut da, &mut db] {
        d.as_object_mut().unwrap().remove("counts");
    }
    assert_eq!(da["messages"], db["messages"]);
    assert_eq!(da["threads"], db["threads"]);
    assert_eq!(da["placements"], db["placements"]);
}

// ---- M2: the write path (§5.3) ----

/// §5.3: "local state changes immediately, the operation is queued".
///
/// Both halves matter, and the test asserts both: the flag is visible before
/// any server has agreed, and the server agrees after the next sync.
#[test]
fn flags_are_local_immediately_and_pushed_on_sync() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let id = w.id("inv42@example.com");
    let out = w.json(&["flag", &id, "+seen", "+flagged"]);
    assert_eq!(out["changed"], 1);

    // Visible now, with no sync in between.
    let shown = w.json(&["show", &id]);
    let flags: Vec<&str> = shown["message"]["flags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(flags.contains(&"seen"), "{flags:?}");
    assert!(flags.contains(&"flagged"), "{flags:?}");

    // And queued, which `status` says.
    let status = w.json(&["status"]);
    assert_eq!(status["accounts"][0]["queued_ops"], 1);

    // The server has not heard yet.
    assert!(w.server_flags("INBOX", "<inv42@example.com>").is_empty());

    let synced = w.sync();
    assert_eq!(synced["synced"][0]["ops_applied"], 1);
    assert_eq!(
        w.server_flags("INBOX", "<inv42@example.com>"),
        vec!["flagged".to_string(), "seen".to_string()]
    );
    assert_eq!(w.json(&["status"])["accounts"][0]["queued_ops"], 0);
}

/// Removing a flag is the same path in reverse, and `-seen` is how a message is
/// marked unread — there is no `unread` flag to add.
#[test]
fn flags_can_be_removed() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    // The reply was seeded `\Seen`.
    let id = w.id("reply@example.com");
    w.json(&["flag", &id, "-seen"]);
    w.sync();
    assert!(!w
        .server_flags("INBOX", "<reply@example.com>")
        .contains(&"seen".to_string()));

    let out = w.run(&["--json", "flag", &id, "+unread"]);
    assert_eq!(
        out.status, 2,
        "`+unread` should be rejected: {}",
        out.stdout
    );
    assert!(out.error_code() == "usage");
}

/// Several changes to one message collapse into one `STORE`.
#[test]
fn repeated_flag_changes_collapse_into_one_operation() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let id = w.id("inv42@example.com");
    w.json(&["flag", &id, "+seen"]);
    w.json(&["flag", &id, "+flagged"]);
    w.json(&["flag", &id, "-seen"]);

    assert_eq!(
        w.json(&["status"])["accounts"][0]["queued_ops"],
        1,
        "three edits to one message should be one queued operation"
    );

    w.sync();
    // Last write wins per flag: `+seen` then `-seen` is unseen.
    assert_eq!(
        w.server_flags("INBOX", "<inv42@example.com>"),
        vec!["flagged".to_string()]
    );
}

#[test]
fn a_move_is_local_immediately_and_pushed_on_sync() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let id = w.id("inv42@example.com");
    w.json(&["mv", &id, "Archive"]);

    // Local view moved before the server heard anything.
    assert_eq!(w.json(&["show", &id])["message"]["folder"], "Archive");
    assert_eq!(
        w.server_count("Archive"),
        1,
        "the server still has only the digest"
    );

    w.sync();
    assert!(w.server_has("Archive", "<inv42@example.com>"));
    assert!(!w.server_has("INBOX", "<inv42@example.com>"));
    // And the store agrees with itself afterwards.
    assert_eq!(w.json(&["show", &id])["message"]["folder"], "Archive");
}

/// `rm`, `archive`, and `spam` resolve the server's own special-use folders, so
/// the user never has to know whether it is `Junk` or `Spam` (§6.1).
#[test]
fn the_move_sugar_resolves_special_use_folders() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    w.json(&["archive", &w.id("inv42@example.com")]);
    w.json(&["spam", &w.id("reply@example.com")]);
    w.json(&["rm", &w.id("digest@example.org")]);
    w.sync();

    assert!(w.server_has("Archive", "<inv42@example.com>"));
    assert!(w.server_has("Junk", "<reply@example.com>"));
    assert!(w.server_has("Trash", "<digest@example.org>"));
}

/// `cp` leaves the original where it was.
#[test]
fn copy_puts_the_message_in_both_folders() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    w.json(&["cp", &w.id("inv42@example.com"), "Archive"]);
    w.sync();

    assert!(w.server_has("Archive", "<inv42@example.com>"));
    assert!(
        w.server_has("INBOX", "<inv42@example.com>"),
        "the original should stay"
    );
}

/// The property that makes a crash mid-push cost nothing (§14.3): replaying an
/// operation that already applied changes neither the server nor the store.
#[test]
fn replaying_the_queue_is_idempotent() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    w.json(&["flag", &w.id("inv42@example.com"), "+seen"]);
    w.json(&["mv", &w.id("digest@example.org"), "INBOX"]);
    w.sync();
    let after_first = w.dump(None);

    let second = w.sync();
    assert_eq!(second["synced"][0]["ops_applied"], 0);
    assert_eq!(second["synced"][0]["messages_added"], 0);
    assert_eq!(second["synced"][0]["messages_removed"], 0);
    assert_eq!(
        w.dump(None),
        after_first,
        "a second pass must change nothing"
    );
}

/// Writes made offline survive and push when the server comes back — which is
/// the whole reason the queue is in `meta.db` rather than in memory.
#[test]
fn writes_made_while_offline_are_pushed_later() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    // Point the account at a dead port, make three edits, and confirm the sync
    // fails without losing them.
    let good = std::fs::read_to_string(&w.config).unwrap();
    std::fs::write(&w.config, good.replace(&format!("{PORT}"), "1")).unwrap();

    w.json(&["flag", &w.id("inv42@example.com"), "+flagged"]);
    w.json(&["mv", &w.id("digest@example.org"), "INBOX"]);
    let offline = w.run(&["--json", "sync", "--once"]);
    assert_ne!(offline.status, 0, "syncing to a dead port should fail");
    assert_eq!(w.json(&["status"])["accounts"][0]["queued_ops"], 2);

    std::fs::write(&w.config, good).unwrap();
    let synced = w.sync();
    assert_eq!(synced["synced"][0]["ops_applied"], 2);
    assert!(w
        .server_flags("INBOX", "<inv42@example.com>")
        .contains(&"flagged".to_string()));
    assert!(w.server_has("INBOX", "<digest@example.org>"));
}

/// §5.3: "conflicts resolve to the server's flags".
///
/// A flag change for a message another client expunged is not an error — IMAP
/// makes `STORE` on a UID that is gone a silent no-op — and the right outcome
/// is not a conflict but a quiet convergence: the push does nothing, the pull
/// removes the message, and the store ends up agreeing with the server rather
/// than holding an edit to something that no longer exists.
#[test]
fn a_write_to_a_vanished_message_resolves_in_the_servers_favour() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    let id = w.id("inv42@example.com");
    w.json(&["flag", &id, "+flagged"]);

    // Another client expunges it out from under us.
    let uid = *w.uids.get("root").expect("a recorded UID");
    w.expunge("INBOX", uid);

    let synced = w.sync();
    assert_eq!(
        synced["synced"][0]["ops_conflicted"], 0,
        "a no-op push is not a conflict"
    );
    assert!(!w.has("inv42@example.com"), "the server's version won");
    assert_eq!(w.json(&["status"])["accounts"][0]["queued_ops"], 0);
}

/// A write the server actually *refuses* is a conflict, and §5.3 requires the
/// note to reach `mailbox status`.
///
/// A move whose destination was deleted between queueing and pushing is the
/// clean case: the server answers `NO`, no retry can help, and the user needs
/// to be told rather than have it retried silently forever.
#[test]
fn a_refused_write_is_reported_as_a_conflict() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.append("digest", "Project", BASIC);
    w.sync();

    // Queue a move into `Project`, then delete it server-side.
    w.json(&["mv", &w.id("inv42@example.com"), "Project"]);
    w.delete_folder("Project");

    let synced = w.sync();
    assert_eq!(synced["synced"][0]["ops_conflicted"], 1);

    let status = w.json(&["status"]);
    let conflicts = status["accounts"][0]["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["operation"], "move");
    assert!(
        !conflicts[0]["reason"].as_str().unwrap().is_empty(),
        "the note has to say why"
    );

    // Abandoned rather than retried: nothing is left queued.
    assert_eq!(status["accounts"][0]["queued_ops"], 0);
}

/// A queued move must survive a `UIDVALIDITY` change: the recorded UID stops
/// meaning anything, and the `Message-ID` search is what finds the message
/// again (§5.2, §5.3).
#[test]
fn a_queued_move_survives_a_uidvalidity_change() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    // Queue the move first, then invalidate every UID in the source folder by
    // recreating it and putting the message back.
    w.json(&["mv", &w.id("digest@example.org"), "INBOX"]);
    w.recreate_folder("Archive");
    w.append("digest", "Archive", BASIC);

    let synced = w.sync();
    assert_eq!(
        synced["synced"][0]["ops_conflicted"], 0,
        "the move should have been re-found, not abandoned"
    );
    assert!(w.server_has("INBOX", "<digest@example.org>"));
}

/// §6.5: `--dry-run` shows exactly what would change and touches nothing —
/// neither the store nor the network.
#[test]
fn dry_run_writes_change_nothing() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();
    let before = w.dump(None);

    let out = w.json(&["--dry-run", "flag", &w.id("inv42@example.com"), "+seen"]);
    assert_eq!(out["dry_run"], true);
    assert_eq!(out["add_flags"][0], "seen");

    let out = w.json(&["--dry-run", "mv", &w.id("inv42@example.com"), "Archive"]);
    assert_eq!(out["target"], "Archive");

    assert_eq!(w.dump(None), before, "a dry run must not touch the store");
    assert_eq!(w.json(&["status"])["accounts"][0]["queued_ops"], 0);
}

/// Tags are local until M8 gives them the state folder (§5.4), so they change
/// the store and queue nothing.
#[test]
fn tags_are_local_and_queue_nothing() {
    l2_or_skip!();
    let mut w = World::new();
    w.seed(BASIC);
    w.sync();

    w.json(&["flag", &w.id("inv42@example.com"), "+tag:invoices"]);
    assert_eq!(
        w.json(&["status"])["accounts"][0]["queued_ops"],
        0,
        "a tag has nowhere to go on an IMAP server"
    );
    let hits = w.json(&["search", "tag:invoices"]);
    assert_eq!(hits["messages"].as_array().unwrap().len(), 1);
}

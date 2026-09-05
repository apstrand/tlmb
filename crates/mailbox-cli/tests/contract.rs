//! The JSON contract (§6.4), end to end through the CLI, with no server.
//!
//! §14.4 makes schema conformance a gating property: breaking the contract must
//! fail a build rather than a user. These tests seed a store from a fixture
//! (§14.1), run the real binary, and check both the *shape* of what comes back
//! — against the schema the binary itself publishes — and the exit codes of
//! §6.6.

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
text = "%PDF-1.4"

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
body = "Paid.\n\nOn Sat, Alice wrote:\n> Please pay the invoice."

[[message]]
name = "digest"
folder = "Archive"
from = "News <news@example.org>"
subject = "Weekly digest"
date = "2026-07-20T08:00:00Z"
message_id = "<digest@example.org>"
flags = ["\\Seen", "\\Flagged"]
html = "<h1>Digest</h1><p>Hello <b>world</b>.</p>"
body = "Digest\n\nHello world."
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

impl Run {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("expected one JSON document, got {e}:\n{}", self.stdout))
    }
    fn error(&self) -> serde_json::Value {
        serde_json::from_str(&self.stderr)
            .unwrap_or_else(|e| panic!("expected a JSON error, got {e}:\n{}", self.stderr))
    }
}

impl Cli {
    fn new() -> Cli {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-contract-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.test]\naddress = \"peter@example.com\"\nidentities = [\"p.strand@example.com\"]\nimap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");

        let cli = Cli {
            state: dir.join("state"),
            config,
            dir,
        };

        let manifest = cli.dir.join("fixture.toml");
        std::fs::write(&manifest, FIXTURE).expect("write fixture");
        // Deliberately no `--yes`: these tests run with stdin redirected, and a
        // state-changing command must not treat that as a missing confirmation.
        let seeded = cli.run(&["debug", "fixture", "load", manifest.to_str().unwrap()]);
        assert_eq!(seeded.status, 0, "seeding failed: {}", seeded.stderr);
        cli
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "contract")
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

    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let run = self.run(&full);
        assert_eq!(run.status, 0, "`{}` failed: {}", full.join(" "), run.stderr);
        run.json()
    }

    /// The stable id of a message, by the `Message-ID` the fixture pinned.
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

/// Checks a document against the schema the binary publishes for that command.
///
/// The binary is both sides of this: `mailbox schema ls` and `mailbox ls
/// --json` come from the same types, so a drift between them means the
/// generated schema stopped describing the data — which is the failure agents
/// would hit first and notice last.
fn assert_conforms(cli: &Cli, command: &str, document: &serde_json::Value) {
    let run = cli.run(&["schema", command]);
    assert_eq!(run.status, 0, "`schema {command}` failed: {}", run.stderr);
    let schema: serde_json::Value = serde_json::from_str(&run.stdout).expect("schema is JSON");

    let validator = jsonschema::validator_for(&schema)
        .unwrap_or_else(|e| panic!("the schema for `{command}` is not a valid JSON Schema: {e}"));
    if let Err(error) = validator.validate(document) {
        panic!(
            "`{command} --json` does not conform to `schema {command}`:\n  {error}\n\ndocument:\n{}",
            serde_json::to_string_pretty(document).unwrap_or_default()
        );
    }
}

#[test]
fn every_command_conforms_to_its_published_schema() {
    let cli = Cli::new();
    let id = cli.id("inv42@example.com");

    for (command, args) in [
        ("ls", vec!["ls"]),
        ("search", vec!["search", "invoice"]),
        ("show", vec!["show", id.as_str()]),
        ("folders", vec!["folders"]),
        ("accounts", vec!["accounts"]),
        ("status", vec!["status"]),
        ("watch", vec!["watch", "--drain", "--cursor", "test:0"]),
    ] {
        let document = cli.ok(&args);
        assert_conforms(&cli, command, &document);
    }
}

#[test]
fn every_success_is_stamped_with_the_schema_version() {
    let cli = Cli::new();
    for args in [
        vec!["ls"],
        vec!["search", "invoice"],
        vec!["folders"],
        vec!["accounts"],
        vec!["status"],
        vec!["watch", "--drain"],
    ] {
        let doc = cli.ok(&args);
        assert_eq!(doc["schema"], "mailbox/v1", "{args:?}");
        assert!(doc["command"].is_string(), "{args:?}");
    }
}

/// §6.4: exactly one JSON document, and nothing else on stdout.
#[test]
fn json_output_is_exactly_one_document() {
    let cli = Cli::new();
    let run = cli.run(&["--json", "ls"]);
    assert_eq!(run.stdout.trim().lines().count(), 1);
    assert!(run.stdout.trim().starts_with('{'));
    // No ANSI, no spinners, no progress bars.
    assert!(!run.stdout.contains('\x1b'));
}

#[test]
fn ndjson_streams_an_envelope_then_rows_then_a_cursor() {
    let cli = Cli::new();
    let run = cli.run(&["--ndjson", "ls", "-n", "1"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let lines: Vec<serde_json::Value> = run
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("NDJSON"))
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["type"], "begin");
    assert_eq!(lines[0]["schema"], "mailbox/v1");
    assert!(lines[1]["id"].is_string());
    assert_eq!(lines[2]["type"], "end");
    assert!(
        lines[2]["cursor"].is_string(),
        "a truncated page should offer a cursor"
    );
}

/// §6.6, exhaustively: each failure gets its own code, and an agent branches on
/// the code rather than on prose.
#[test]
fn exit_codes_match_the_spec() {
    let cli = Cli::new();
    let cases: &[(&str, i32, Vec<&str>)] = &[
        ("usage", 2, vec!["--json", "search", "form:alice"]),
        ("usage", 2, vec!["--json", "ls", "--nonexistent-flag"]),
        ("not_found", 3, vec!["--json", "show", "ffffff0"]),
        ("not_found", 3, vec!["--json", "--account", "nope", "ls"]),
        ("not_found", 3, vec!["--json", "schema", "nonsense"]),
    ];
    for (code, status, args) in cases {
        let run = cli.run(args);
        assert_eq!(run.status, *status, "`{}`:\n{}", args.join(" "), run.stderr);
        assert_eq!(run.error()["error"]["code"], *code, "`{}`", args.join(" "));
    }
}

/// §6.4: failures are structured, on stderr, and never a bare English sentence.
#[test]
fn errors_are_structured_even_when_the_arguments_never_parsed() {
    let cli = Cli::new();
    let run = cli.run(&["--json", "ls", "--nonexistent-flag"]);
    let err = run.error();
    assert_eq!(err["error"]["code"], "usage");
    assert!(err["error"]["message"].is_string());
    assert_eq!(err["error"]["retryable"], false);
    assert!(
        err["error"]["hint"].is_string(),
        "a usage error should say where to look"
    );
    assert!(
        run.stdout.is_empty(),
        "nothing belongs on stdout when a command fails"
    );
}

#[test]
fn error_documents_conform_to_the_error_schema() {
    let cli = Cli::new();
    let run = cli.run(&["--json", "show", "ffffff0"]);
    assert_conforms(&cli, "error", &run.error());
}

/// §6.4's non-TTY rule is about commands that *would have prompted*, and none
/// of M1's do. Treating a redirected stdin as a missing confirmation broke
/// `mailbox sync` from cron, from a systemd timer, and from every script —
/// which is most of the ways a sync is meant to be run.
#[test]
fn a_redirected_stdin_is_not_a_missing_confirmation() {
    let cli = Cli::new();
    // `--dry-run` so this asserts on the gate rather than on the network.
    let run = cli.run(&["--json", "sync", "--once", "--dry-run"]);
    assert_eq!(
        run.status, 0,
        "sync without --yes must work from a script:\n{}",
        run.stderr
    );
    assert_eq!(run.json()["would_sync"], true);
}

/// §6.4: read-only by default in agent mode.
#[test]
fn agent_mode_gates_state_changing_commands() {
    let cli = Cli::new();
    let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(["--json", "sync", "--once"])
        .env("MAILBOX_CONFIG", &cli.config)
        .env("MAILBOX_STATE_DIR", &cli.state)
        .env("MAILBOX_AGENT", "1")
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(8),
        "§6.6 code 8 is needs_confirmation"
    );
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).expect("a structured error");
    assert_eq!(err["error"]["code"], "needs_confirmation");
    assert!(
        err["error"]["hint"].as_str().unwrap().contains("--yes"),
        "the error must name the flag that supplies the answer"
    );

    // Reading is never gated.
    let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(["--json", "ls"])
        .env("MAILBOX_CONFIG", &cli.config)
        .env("MAILBOX_STATE_DIR", &cli.state)
        .env("MAILBOX_AGENT", "1")
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn fields_projects_rows_but_keeps_the_envelope() {
    let cli = Cli::new();
    let doc = cli.ok(&["ls", "--fields", "id,subject"]);
    assert_eq!(
        doc["schema"], "mailbox/v1",
        "the envelope must survive a projection"
    );
    for m in doc["messages"].as_array().unwrap() {
        let keys: Vec<&String> = m.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(m["id"].is_string());
        assert!(m["subject"].is_string());
    }
}

/// Cursors, not offsets: paging must visit every row exactly once.
#[test]
fn paging_with_cursors_is_total() {
    let cli = Cli::new();
    let all: Vec<String> = cli.ok(&["ls", "in:inbox OR in:archive", "-n", "100"])["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(all.len(), 3);

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = vec![
            "ls".to_string(),
            "in:inbox OR in:archive".to_string(),
            "-n".into(),
            "1".into(),
        ];
        if let Some(c) = &cursor {
            args.push("--cursor".into());
            args.push(c.clone());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let page = cli.ok(&refs);
        for m in page["messages"].as_array().unwrap() {
            seen.push(m["id"].as_str().unwrap().to_string());
        }
        match page["cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, all);
}

#[test]
fn the_token_aware_renderings_work() {
    let cli = Cli::new();

    // --strip-quotes drops the quoted reply and its attribution.
    let doc = cli.ok(&["show", &cli.id("reply@example.com"), "--strip-quotes"]);
    let body = doc["message"]["body"]["content"].as_str().unwrap();
    assert_eq!(body.trim(), "Paid.");

    // --format=md converts HTML mail to Markdown.
    let doc = cli.ok(&["show", &cli.id("digest@example.org"), "--format=md"]);
    let body = doc["message"]["body"]["content"].as_str().unwrap();
    assert!(body.contains("# Digest"), "{body}");
    assert!(body.contains("**world**"), "{body}");

    // --max-body-bytes truncates and says so explicitly.
    let doc = cli.ok(&[
        "show",
        &cli.id("inv42@example.com"),
        "--max-body-bytes",
        "5",
    ]);
    assert_eq!(doc["message"]["body"]["truncated"], true);
    assert!(doc["message"]["body"]["content"].as_str().unwrap().len() <= 5);

    // --no-attachments omits the list.
    let doc = cli.ok(&["show", &cli.id("inv42@example.com"), "--no-attachments"]);
    assert!(doc["message"]["attachments"].as_array().unwrap().is_empty());

    // --headers-only omits the body.
    let doc = cli.ok(&["show", &cli.id("inv42@example.com"), "--headers-only"]);
    assert!(doc["message"]["body"].is_null());
}

/// §13: `show --raw` emits the exact bytes, not a JSON document.
#[test]
fn show_raw_emits_rfc5322_bytes() {
    let cli = Cli::new();
    let run = cli.run(&["show", &cli.id("inv42@example.com"), "--raw"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(
        run.stdout.starts_with("Date: "),
        "{}",
        &run.stdout[..60.min(run.stdout.len())]
    );
    assert!(run.stdout.contains("Message-ID: <inv42@example.com>"));
    assert!(run.stdout.contains("Please pay the invoice."));
}

/// §5.2's conveniences: `^` names the last message printed, and `1..20` indexes
/// the last listing.
#[test]
fn the_listing_shorthands_work() {
    let cli = Cli::new();
    let first = cli.ok(&["ls"])["messages"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let by_caret = cli.ok(&["show", "^"]);
    assert_eq!(by_caret["message"]["id"], first);

    // Re-list, because `show` moved `^` to what it showed.
    cli.ok(&["ls"]);
    let by_index = cli.ok(&["show", "1"]);
    assert_eq!(by_index["message"]["id"], first);
}

/// `@` names the TUI/GUI selection, and neither exists yet. Saying so is better
/// than inventing an answer.
#[test]
fn the_selection_shorthand_says_it_needs_a_running_interface() {
    let cli = Cli::new();
    let run = cli.run(&["--json", "show", "@"]);
    assert_eq!(run.status, 2);
    assert!(run.error()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("TUI"));
}

#[test]
fn human_output_is_aligned_and_free_of_ansi_when_no_color_is_set() {
    let cli = Cli::new();
    let run = cli.run(&["ls"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(!run.stdout.contains('\x1b'), "NO_COLOR must be honoured");
    assert!(
        run.stdout.contains("subject"),
        "the header row should be there"
    );
    assert!(run.stdout.contains("Invoice 42"));
    // Terminal-width-aware: nothing may exceed COLUMNS.
    for line in run.stdout.lines() {
        assert!(
            line.chars().count() <= 100,
            "line exceeds COLUMNS: {line:?}"
        );
    }
}

#[test]
fn store_dump_is_ordered_and_free_of_server_assigned_values() {
    let cli = Cli::new();
    let dump: serde_json::Value =
        serde_json::from_str(&cli.run(&["debug", "store-dump"]).stdout).expect("dump");

    let ids: Vec<&str> = dump["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "the dump must be ordered by stable id");

    // Placements are keyed by folder path and stable id — never by UID, which
    // two devices would disagree about (§14.3).
    for p in dump["placements"].as_array().unwrap() {
        assert!(p["folder"].is_string());
        assert!(p["id"].is_string());
        assert!(
            p.get("uid").is_none(),
            "a UID in the dump would make it device-specific"
        );
    }
}

#[test]
fn debug_clock_reports_the_injected_source() {
    let cli = Cli::new();
    let run = cli.run(&["debug", "clock"]);
    let doc: serde_json::Value = serde_json::from_str(&run.stdout).expect("JSON");
    assert_eq!(doc["source"], "MAILBOX_CLOCK");
    assert_eq!(doc["rfc3339"], "2026-09-01T12:00:00Z");
    assert_eq!(doc["seed"], "contract");
}

/// An account that has never synced is not an error: `ls` says "no messages"
/// and `status` reports zeroes.
#[test]
fn a_store_that_does_not_exist_yet_reads_as_empty() {
    let cli = Cli::new();
    let empty = cli.dir.join("empty-state");
    let run = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(["--json", "ls"])
        .env("MAILBOX_CONFIG", &cli.config)
        .env("MAILBOX_STATE_DIR", &empty)
        .output()
        .expect("run");
    assert_eq!(
        run.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&run.stdout).expect("JSON");
    assert_eq!(doc["messages"].as_array().unwrap().len(), 0);
    // And it must not have created anything on disk: a read is a read (§4.1).
    assert!(!empty.join("test/meta.db").exists());
}

#[test]
fn schema_with_no_argument_lists_the_commands() {
    let cli = Cli::new();
    let doc = cli.ok(&["schema"]);
    let commands: Vec<&str> = doc["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for expected in [
        "ls", "search", "show", "folders", "accounts", "sync", "status", "watch",
    ] {
        assert!(
            commands.contains(&expected),
            "{expected} should have a schema"
        );
    }
}

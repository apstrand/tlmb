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

[[folder]]
path = "Trash"
special_use = "trash"

[[folder]]
path = "Junk"
special_use = "junk"

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

    for (command, coverage) in COVERAGE {
        let Coverage::Checked(args) = coverage else {
            continue;
        };
        let owned: Vec<String> = args.iter().map(|a| a.replace("<id>", &id)).collect();
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        let document = cli.ok(&args);
        assert_conforms(&cli, command, &document);
    }
}

/// What §14.4 requires of one subcommand.
///
/// M7 makes schema conformance gating "for every command", which is a claim
/// about the *set* as much as about each member: the point of the table below
/// is that a new subcommand cannot be added without landing in it, and that
/// declaring one exempt means writing down why.
enum Coverage {
    /// Runs offline, so both halves are checked: the schema exists, and real
    /// output validates against it.
    Checked(&'static [&'static str]),
    /// Publishes a schema, but needs a server to produce a document. The L2
    /// and L3 suites are where those documents get checked.
    SchemaOnly,
    /// Emits no JSON document at all. The string is the reason.
    NoDocument(&'static str),
}

const COVERAGE: &[(&str, Coverage)] = &[
    ("ls", Coverage::Checked(&["ls"])),
    ("search", Coverage::Checked(&["search", "invoice"])),
    ("show", Coverage::Checked(&["show", "<id>"])),
    ("folders", Coverage::Checked(&["folders"])),
    ("accounts", Coverage::Checked(&["accounts"])),
    ("status", Coverage::Checked(&["status"])),
    ("snooze", Coverage::Checked(&["snooze", "--list"])),
    (
        "watch",
        Coverage::Checked(&["watch", "--drain", "--cursor", "test:0"]),
    ),
    ("notify", Coverage::Checked(&["notify"])),
    ("outbox", Coverage::Checked(&["outbox"])),
    ("attach", Coverage::Checked(&["attach", "<id>"])),
    ("flag", Coverage::Checked(&["flag", "<id>", "+seen"])),
    ("mv", Coverage::Checked(&["mv", "<id>", "Archive"])),
    ("cp", Coverage::Checked(&["cp", "<id>", "Archive"])),
    ("rm", Coverage::Checked(&["rm", "<id>"])),
    ("archive", Coverage::Checked(&["archive", "<id>"])),
    ("spam", Coverage::Checked(&["spam", "<id>"])),
    (
        "send",
        Coverage::Checked(&[
            "send",
            "--dry-run",
            "--to",
            "someone@example.org",
            "--subject",
            "s",
            "--body",
            "b",
        ]),
    ),
    // Opens $EDITOR and then goes down `send`'s path, which is checked above.
    ("compose", Coverage::SchemaOnly),
    // Needs a message to reply to *and* $EDITOR; `send`'s schema covers the
    // document it produces.
    ("reply", Coverage::SchemaOnly),
    // Talks to a server by definition. The L3 suite runs it for real.
    ("sync", Coverage::SchemaOnly),
    (
        "serve",
        Coverage::NoDocument("runs a web server until interrupted"),
    ),
    (
        "relay",
        Coverage::NoDocument("runs a pairing relay until interrupted"),
    ),
    (
        "site",
        Coverage::NoDocument("writes a static hosted bundle, not a mailbox reply"),
    ),
    ("gui", Coverage::NoDocument("opens a window")),
    ("tui", Coverage::NoDocument("takes over the terminal")),
    (
        "mcp",
        Coverage::NoDocument("speaks JSON-RPC on stdio, not the envelope"),
    ),
    (
        "pair",
        Coverage::NoDocument("a two-device conversation; `tests/pairing.rs` drives it"),
    ),
    (
        "schema",
        Coverage::NoDocument("emits a JSON Schema, which is not a Reply"),
    ),
    ("help", Coverage::NoDocument("clap's own")),
];

/// The gate itself: every subcommand the binary offers is classified above.
///
/// Derived from `--help` rather than a second hand-written list, because a
/// list that has to be updated by hand is one that will not be.
#[test]
fn every_subcommand_is_accounted_for() {
    let cli = Cli::new();
    let help = cli.run(&["--help"]);
    let mut commands = Vec::new();
    let mut in_commands = false;
    for line in help.stdout.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            if line.trim().is_empty() {
                break;
            }
            if let Some(name) = line.split_whitespace().next() {
                commands.push(name.to_string());
            }
        }
    }
    assert!(
        commands.len() > 20,
        "could not read the command list: {help:?}",
        help = help.stdout
    );

    for command in &commands {
        assert!(
            COVERAGE.iter().any(|(name, _)| name == command),
            "`{command}` is a new subcommand with no entry in COVERAGE — \
             give it a schema, or say in the table why it has no document (§14.4)"
        );
    }
    for (name, _) in COVERAGE {
        assert!(
            commands.contains(&name.to_string()),
            "COVERAGE lists `{name}`, which the binary no longer offers"
        );
    }
}

/// And everything the table says has a schema, has one.
#[test]
fn every_documented_command_publishes_a_schema() {
    let cli = Cli::new();
    for (command, coverage) in COVERAGE {
        if let Coverage::NoDocument(why) = coverage {
            // And the converse, so a command that grows a document is
            // reclassified rather than quietly left out of the gate.
            let run = cli.run(&["schema", command]);
            assert_ne!(
                run.status, 0,
                "`{command}` publishes a schema, but COVERAGE says it emits no document ({why})"
            );
            continue;
        }
        let run = cli.run(&["schema", command]);
        assert_eq!(
            run.status, 0,
            "`schema {command}` failed, but the table says it emits a document: {}",
            run.stderr
        );
        assert!(
            serde_json::from_str::<serde_json::Value>(&run.stdout).is_ok(),
            "`schema {command}` did not print a schema"
        );
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

//! `mailbox mcp` (§6.4): the contract as MCP tools, over stdio.
//!
//! The protocol is the easy part. What these tests are really about is the
//! read/write split — §6.4 makes agent mode read-only by default, and an MCP
//! server is agent mode by definition — and §6.5's guardrails on sending,
//! which must apply here exactly as they do to `mailbox send` because there is
//! one send path and `mcp` does not have its own.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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
html = "<p>Please pay the <a href=\"https://example.com/i/42\">invoice</a>.</p>"
body = "Please pay the invoice."
"#;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl World {
    fn new() -> World {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-mcp-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");

        let world = World {
            state: dir.join("state"),
            config,
            dir,
        };
        let manifest = world.dir.join("fixture.toml");
        std::fs::write(&manifest, FIXTURE).expect("write fixture");
        let seeded = world
            .command(&["debug", "fixture", "load", manifest.to_str().unwrap()])
            .output()
            .expect("seed");
        assert!(seeded.status.success(), "seeding failed");
        world
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "mcp")
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

    fn server(&self, extra: &[&str]) -> Server {
        let mut args = vec!["mcp"];
        args.extend_from_slice(extra);
        let mut child = self
            .command(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start the server");
        let out = BufReader::new(child.stdout.take().expect("stdout"));
        Server {
            child,
            out,
            next_id: 1,
        }
    }

    fn id(&self, message_id: &str) -> String {
        let out = self
            .command(&["debug", "store-dump"])
            .output()
            .expect("dump");
        let dump: serde_json::Value = serde_json::from_slice(&out.stdout).expect("dump is JSON");
        dump["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["message_id"].as_str() == Some(message_id))
            .expect("the message")["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Server {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Server {
    /// One request, one response. Every test here is request/response, and a
    /// helper that pairs them is what keeps the ids from drifting.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{line}").expect("write");
        stdin.flush().expect("flush");

        let mut response = String::new();
        self.out.read_line(&mut response).expect("read");
        let value: serde_json::Value = serde_json::from_str(&response)
            .unwrap_or_else(|e| panic!("expected JSON-RPC, got {e}: {response}"));
        assert_eq!(value["id"], id, "responses must answer their request");
        assert_eq!(value["jsonrpc"], "2.0");
        value
    }

    fn notify(&mut self, method: &str) {
        let stdin = self.child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc": "2.0", "method": method})
        )
        .expect("write");
        stdin.flush().expect("flush");
    }

    fn tools(&mut self) -> Vec<String> {
        self.request("tools/list", serde_json::json!({}))["result"]["tools"]
            .as_array()
            .expect("a tool list")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Calls a tool and returns its parsed document, asserting it succeeded.
    fn call(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.raw_call(name, arguments);
        assert!(
            response["result"]["isError"].as_bool() != Some(true),
            "`{name}` failed: {}",
            response
        );
        response["result"]["structuredContent"].clone()
    }

    fn raw_call(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
    }

    /// The error document a failed call carries, which is §6.6's.
    fn call_error(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.raw_call(name, arguments);
        assert_eq!(
            response["result"]["isError"], true,
            "`{name}` should have failed: {response}"
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        serde_json::from_str(text).expect("the error is a JSON document")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---- the protocol ----

#[test]
fn it_initializes_and_answers_the_version_the_client_asked_for() {
    let world = World::new();
    let mut server = world.server(&[]);
    let reply = server.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" },
        }),
    );
    assert_eq!(reply["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(reply["result"]["serverInfo"]["name"], "mailbox");
    assert!(reply["result"]["capabilities"]["tools"].is_object());

    // A version nobody has heard of gets ours back, rather than an echo that
    // would promise to speak something we do not.
    let mut server = world.server(&[]);
    let reply = server.request(
        "initialize",
        serde_json::json!({ "protocolVersion": "1999-01-01", "capabilities": {} }),
    );
    assert_ne!(reply["result"]["protocolVersion"], "1999-01-01");
}

#[test]
fn a_notification_is_answered_with_silence() {
    let world = World::new();
    let mut server = world.server(&[]);
    server.notify("notifications/initialized");
    // If the notification had produced a line, this would read *it* rather
    // than the reply to `ping`, and the id assertion would fail.
    let reply = server.request("ping", serde_json::json!({}));
    assert!(reply["result"].is_object());
}

#[test]
fn an_unknown_method_is_a_protocol_error() {
    let world = World::new();
    let mut server = world.server(&[]);
    let reply = server.request("resources/list", serde_json::json!({}));
    assert_eq!(reply["error"]["code"], -32601);
}

// ---- the read/write split (§6.4) ----

#[test]
fn without_yes_the_writing_tools_are_not_even_offered() {
    let world = World::new();
    let mut server = world.server(&[]);
    let tools = server.tools();
    assert!(tools.contains(&"list_messages".to_string()));
    for write in [
        "flag_message",
        "move_message",
        "send_message",
        "sync_mailbox",
    ] {
        assert!(
            !tools.contains(&write.to_string()),
            "{write} was offered by a read-only server: {tools:?}"
        );
    }
}

#[test]
fn a_write_tool_called_by_name_anyway_is_refused() {
    let world = World::new();
    let mut server = world.server(&[]);
    let id = world.id("inv42@example.com");
    let error = server.call_error(
        "flag_message",
        serde_json::json!({ "ids": [id], "add": ["seen"] }),
    );
    assert_eq!(error["error"]["code"], "needs_confirmation");
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap_or_default()
            .contains("--yes"),
        "and it should say how: {error}"
    );
}

#[test]
fn with_yes_the_writing_tools_are_there_and_work() {
    let world = World::new();
    let mut server = world.server(&["--yes"]);
    assert!(server.tools().contains(&"flag_message".to_string()));

    let id = world.id("inv42@example.com");
    let result = server.call(
        "flag_message",
        serde_json::json!({ "ids": [id], "add": ["seen"] }),
    );
    assert_eq!(result["changed"], 1);
    assert_eq!(result["messages"][0]["flags"], serde_json::json!(["seen"]));
}

// ---- reading ----

#[test]
fn listing_and_searching_are_the_same_documents_the_cli_prints() {
    let world = World::new();
    let mut server = world.server(&[]);
    let listed = server.call("list_messages", serde_json::json!({ "limit": 10 }));
    assert_eq!(listed["messages"].as_array().unwrap().len(), 1);

    let found = server.call("list_messages", serde_json::json!({ "query": "invoice" }));
    assert_eq!(found["messages"][0]["subject"], "Invoice 42");

    // §6.2's rule survives the trip: an unknown key is an error, not zero
    // results that a model would read as "no such mail".
    let error = server.call_error(
        "list_messages",
        serde_json::json!({ "query": "colour:red" }),
    );
    assert_eq!(error["error"]["code"], "usage");
}

#[test]
fn showing_a_message_defaults_to_markdown_so_links_survive() {
    let world = World::new();
    let mut server = world.server(&[]);
    let id = world.id("inv42@example.com");
    let shown = server.call("show_message", serde_json::json!({ "id": &id[..7] }));
    let body = shown["message"]["body"]["content"]
        .as_str()
        .expect("a body");
    assert!(
        body.contains("https://example.com/i/42"),
        "a model that cannot see the link cannot follow it: {body}"
    );
}

#[test]
fn watch_events_returns_a_cursor_that_resumes() {
    let world = World::new();
    let mut server = world.server(&["--yes"]);
    let first = server.call("watch_events", serde_json::json!({}));
    let cursor = first["cursor"].as_str().expect("a cursor").to_string();
    assert!(
        !first["changes"].as_array().unwrap().is_empty(),
        "the arrival"
    );
    assert_eq!(first["truncated"], false);

    let id = world.id("inv42@example.com");
    server.call(
        "flag_message",
        serde_json::json!({ "ids": [id], "add": ["flagged"] }),
    );

    let next = server.call("watch_events", serde_json::json!({ "cursor": cursor }));
    let changes = next["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "only what happened since: {changes:#?}");
    assert_eq!(changes[0]["event"], "flag");
    // Its own write, which is how a loop avoids reacting to itself.
    assert_eq!(changes[0]["local"], true);
}

// ---- §6.5: sending ----

#[test]
fn sending_queues_for_approval_rather_than_going_out() {
    let world = World::new();
    let mut server = world.server(&["--yes"]);
    let result = server.call(
        "send_message",
        serde_json::json!({
            "to": ["someone@example.org"],
            "subject": "Re: Invoice 42",
            "body": "On its way.",
        }),
    );
    assert_eq!(
        result["status"], "pending_approval",
        "an unsupervised sender must not be able to send: {result}"
    );

    // And it really is in the outbox, waiting for a person.
    let outbox = server.call("list_outbox", serde_json::json!({}));
    assert_eq!(outbox["messages"][0]["state"], "pending_approval");
}

#[test]
fn an_idempotency_key_makes_a_retry_return_the_first_result() {
    let world = World::new();
    let mut server = world.server(&["--yes"]);
    let args = serde_json::json!({
        "to": ["someone@example.org"],
        "subject": "Only once",
        "body": "…",
        "idempotency_key": "abc-123",
    });
    let first = server.call("send_message", args.clone());
    let again = server.call("send_message", args);
    assert_eq!(
        first["id"], again["id"],
        "a retry must not queue a second copy"
    );
    assert_eq!(again["duplicate"], true);

    let outbox = server.call("list_outbox", serde_json::json!({}));
    assert_eq!(outbox["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn a_policy_denial_names_the_rule() {
    let world = World::new();
    std::fs::write(
        world.dir.join("policy.toml"),
        "[send]\ndeny_recipients = [\"*@example.org\"]\n",
    )
    .expect("write policy");

    let mut server = world.server(&["--yes"]);
    let error = server.call_error(
        "send_message",
        serde_json::json!({ "to": ["someone@example.org"], "body": "no" }),
    );
    assert_eq!(error["error"]["code"], "policy_denied");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("deny_recipients"),
        "a denial that does not say which rule is a dead end: {error}"
    );
}

#[test]
fn a_dry_run_renders_the_bytes_and_sends_nothing() {
    let world = World::new();
    let mut server = world.server(&["--yes"]);
    let result = server.call(
        "send_message",
        serde_json::json!({
            "to": ["someone@example.org"],
            "subject": "Look but do not touch",
            "body": "…",
            "dry_run": true,
        }),
    );
    assert_eq!(result["status"], "dry_run");
    assert!(result["raw"]
        .as_str()
        .unwrap_or_default()
        .contains("Subject:"));
    assert!(
        server.call("list_outbox", serde_json::json!({}))["messages"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a dry run must leave nothing behind"
    );
}

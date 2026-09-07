//! **L3** (§14.2): the write path against a server that misbehaves on request.
//!
//! > L3 is our own server, in-tree, because real servers are too well-behaved
//! > to reach the recovery paths.
//!
//! Every test here drives the real binary against [`mailbox_l3::SmtpServer`] on
//! a loopback ephemeral port. Nothing leaves the machine, and the server counts
//! deliveries, which is the only way to prove a retry did or did not duplicate.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use mailbox_l3::{Fault, SmtpServer, Stage};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// One test's world: a store, a config pointed at the L3 server, and no IMAP.
///
/// Sending needs no IMAP server at all — only filing the `Sent` copy does — so
/// these tests run without a container and in microseconds.
struct World {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
    server: SmtpServer,
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
        let server = SmtpServer::start().expect("start the L3 server");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-l3-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[accounts.test]\n\
                 address = \"peter@example.com\"\n\
                 imap_host = \"127.0.0.1\"\n\
                 imap_port = 1\n\
                 imap_security = \"none\"\n\
                 smtp_host = \"127.0.0.1\"\n\
                 smtp_port = {}\n\
                 smtp_security = \"none\"\n\
                 require_tls = false\n",
                server.port()
            ),
        )
        .expect("write config");

        World {
            state: dir.join("state"),
            config,
            dir,
            server,
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            // A pinned clock and seed, so Message-IDs and boundaries are
            // reproducible and nothing depends on wall-clock time (§6.7).
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "l3")
            .env("XDG_RUNTIME_DIR", &self.dir)
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

    /// Queues one message and returns its outbox id and `Message-ID`.
    fn queue(&self, subject: &str) -> (i64, String) {
        let result = self.json(&[
            "send",
            "--to",
            "alice@example.com",
            "--subject",
            subject,
            "--body",
            "Hello.",
        ]);
        let id = result["id"].as_i64().expect("an outbox id");
        // The Message-ID is in the built bytes; a dry run of the same inputs
        // under the same seed produces the same one.
        let raw = self.json(&[
            "--dry-run",
            "send",
            "--to",
            "alice@example.com",
            "--subject",
            subject,
            "--body",
            "Hello.",
        ]);
        let message_id = raw["raw"]
            .as_str()
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("Message-ID: "))
            .expect("a Message-ID")
            .trim()
            .to_string();
        (id, message_id)
    }

    /// Runs as an unsupervised agent (§6.4), which is what turns on §6.5's
    /// approval default.
    fn agent(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_PASSWORD_TEST", "testpass")
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("MAILBOX_SEED", "l3")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            // Nothing here may put a popup on the desktop of
            // whoever ran the suite (§11).
            .env("MAILBOX_NOTIFIER", "none")
            .env("MAILBOX_AGENT", "1")
            .output()
            .expect("run");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn flush(&self) -> serde_json::Value {
        self.json(&["outbox", "flush"])
    }

    fn outbox(&self) -> Vec<serde_json::Value> {
        self.json(&["outbox", "list"])["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn entry(&self, id: i64) -> serde_json::Value {
        self.outbox()
            .into_iter()
            .find(|m| m["id"].as_i64() == Some(id))
            .unwrap_or_else(|| panic!("outbox entry {id} is gone"))
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_happy_path_delivers_exactly_once() {
    let w = World::new();
    let (id, message_id) = w.queue("Hello there");

    let report = w.flush();
    assert_eq!(report["sent"], 1);
    assert_eq!(report["failed"], 0);

    assert_eq!(w.server.deliveries_of(&message_id), 1);
    assert_eq!(w.entry(id)["state"], "sent");

    // The envelope is what was on the wire, headers or no headers.
    let received = w.server.received();
    assert_eq!(received[0].from, "peter@example.com");
    assert_eq!(received[0].to, vec!["alice@example.com".to_string()]);
}

/// A `Bcc` recipient must reach the envelope and no header. This is the whole
/// of what makes a blind copy blind, and it is worth a test that reads the
/// bytes the server actually got.
#[test]
fn bcc_reaches_the_envelope_and_no_header() {
    let w = World::new();
    w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--bcc",
        "hidden@example.com",
        "--subject",
        "Quiet",
        "--body",
        "Hello.",
    ]);
    w.flush();

    let received = w.server.received();
    assert_eq!(received.len(), 1);
    assert!(received[0].to.contains(&"hidden@example.com".to_string()));

    let body = String::from_utf8_lossy(&received[0].body);
    assert!(
        !body.to_lowercase().contains("hidden@example.com"),
        "the Bcc address must not appear in the message bytes:\n{body}"
    );
}

/// §14.3's dedicated test, and §16's decision made visible.
///
/// The server keeps the message and hangs up before the `250`. The client
/// cannot tell that from a message that never arrived, so it retries — and the
/// retry delivers a second copy. That is the accepted outcome, and asserting it
/// pins the behaviour rather than wishing it away: if someone later makes the
/// outbox stop retrying here, this test says so.
#[test]
fn the_double_send_window_retries_and_may_duplicate() {
    let w = World::new();
    let (id, message_id) = w.queue("Ambiguous");

    w.server.set_fault(Fault::DropBeforeFinalReply);
    let first = w.flush();
    assert_eq!(first["sent"], 0, "the client was never told it succeeded");
    assert_eq!(first["deferred"], 1, "and treats it as retryable");

    // It really was delivered — the server has it.
    assert_eq!(w.server.deliveries_of(&message_id), 1);
    // And the entry is still queued, so the message is not lost.
    assert_eq!(w.entry(id)["state"], "queued");

    // Now let the retry through.
    w.server.set_fault(Fault::None);
    let second = w.json(&["outbox", "retry", &id.to_string()]);
    assert_eq!(second["state"], "queued");
    let report = w.flush();
    assert_eq!(report["sent"], 1);

    // Two deliveries: the decision of §16, asserted rather than assumed.
    assert_eq!(
        w.server.deliveries_of(&message_id),
        2,
        "the accepted trade is a duplicate rather than a silent loss"
    );
    assert_eq!(w.entry(id)["state"], "sent");
}

/// §6.5's other half: a *caller* retrying `send` with the same key must not
/// enqueue a second message, whatever the network did.
#[test]
fn an_idempotency_key_stops_the_caller_double_queueing() {
    let w = World::new();
    let args = [
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Once",
        "--body",
        "Hello.",
        "--idempotency-key",
        "abc-123",
    ];

    let first = w.json(&args);
    assert_eq!(first["duplicate"], false);
    let second = w.json(&args);
    assert_eq!(second["duplicate"], true);
    assert_eq!(second["id"], first["id"], "the same entry comes back");

    assert_eq!(w.outbox().len(), 1, "one key, one message");
    w.flush();
    assert_eq!(w.server.received().len(), 1);
}

/// The key survives the message having already been sent — which is the case
/// that matters, since an agent retries after a timeout, not before.
#[test]
fn an_idempotency_key_replays_the_result_after_delivery() {
    let w = World::new();
    let args = [
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Once",
        "--body",
        "Hello.",
        "--idempotency-key",
        "xyz-789",
    ];
    let first = w.json(&args);
    w.flush();
    assert_eq!(w.server.received().len(), 1);

    let replay = w.json(&args);
    assert_eq!(replay["duplicate"], true);
    assert_eq!(replay["id"], first["id"]);
    w.flush();
    assert_eq!(
        w.server.received().len(),
        1,
        "a replayed key must not send again"
    );
}

/// A 4xx is temporary: back off and try again, do not lose the mail.
#[test]
fn a_transient_refusal_is_retried() {
    for stage in [Stage::MailFrom, Stage::Rcpt, Stage::Data] {
        let w = World::new();
        let (id, message_id) = w.queue("Deferred");

        w.server.set_fault(Fault::Transient(stage));
        let report = w.flush();
        assert_eq!(report["sent"], 0, "{stage:?}");
        assert_eq!(report["deferred"], 1, "{stage:?}");
        assert_eq!(w.entry(id)["state"], "queued", "{stage:?}");
        assert_eq!(w.server.deliveries_of(&message_id), 0, "{stage:?}");

        // The backoff is real: the entry is not immediately due again.
        let immediate = w.flush();
        assert_eq!(immediate["attempted"], 0, "{stage:?}: backoff not honoured");

        // Once it is allowed through, it goes.
        w.server.set_fault(Fault::None);
        w.json(&["outbox", "retry", &id.to_string()]);
        assert_eq!(w.flush()["sent"], 1, "{stage:?}");
        assert_eq!(w.server.deliveries_of(&message_id), 1, "{stage:?}");
    }
}

/// A 5xx is permanent: stop, and say so. Retrying a "no such mailbox" forever
/// is how an outbox fills up with mail nobody will ever look at.
#[test]
fn a_permanent_refusal_fails_immediately() {
    let w = World::new();
    let (id, message_id) = w.queue("Rejected");

    w.server.set_fault(Fault::Permanent(Stage::Rcpt));
    let report = w.flush();
    assert_eq!(report["failed"], 1);
    assert_eq!(report["deferred"], 0);

    let entry = w.entry(id);
    assert_eq!(entry["state"], "failed");
    assert!(
        entry["last_error"].as_str().unwrap().contains("550"),
        "the error should carry the server's own words: {entry}"
    );
    assert_eq!(w.server.deliveries_of(&message_id), 0);
    assert_eq!(entry["attempts"], 1, "a 5xx should not be attempted twice");
}

/// A connection cut partway through the body delivered nothing, so retrying is
/// safe and must not be confused with the double-send window.
#[test]
fn a_drop_during_the_body_is_safely_retried() {
    let w = World::new();
    let (id, message_id) = w.queue("Interrupted");

    w.server.set_fault(Fault::DropDuringData);
    let report = w.flush();
    assert_eq!(report["sent"], 0);
    assert_eq!(
        w.server.deliveries_of(&message_id),
        0,
        "nothing was delivered"
    );

    w.server.set_fault(Fault::None);
    w.json(&["outbox", "retry", &id.to_string()]);
    w.flush();
    assert_eq!(
        w.server.deliveries_of(&message_id),
        1,
        "exactly one copy after a safe retry"
    );
}

/// §9: TLS is required by default, and a server that advertises `STARTTLS` and
/// then refuses it must not be silently downgraded to plaintext.
#[test]
fn a_refused_starttls_is_not_downgraded() {
    let w = World::new();
    std::fs::write(
        &w.config,
        std::fs::read_to_string(&w.config)
            .unwrap()
            .replace("smtp_security = \"none\"", "smtp_security = \"starttls\"")
            .replace("require_tls = false", "require_tls = true"),
    )
    .unwrap();

    w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Secret",
        "--body",
        "Hello.",
    ]);
    w.server.set_fault(Fault::RefuseStartTls);

    let run = w.run(&["--json", "outbox", "flush"]);
    assert_ne!(run.status, 0, "a refused STARTTLS must not be ignored");
    assert!(
        w.server.received().is_empty(),
        "nothing may go out in plaintext after a failed STARTTLS"
    );
}

/// A wrong credential is `auth_failed` and exit 4 (§6.6), and nothing is sent.
#[test]
fn a_rejected_auth_sends_nothing() {
    let w = World::new();
    w.server.require_auth(Some("something-else".into()));
    w.queue("Unauthorized");

    let run = w.run(&["--json", "outbox", "flush"]);
    assert_eq!(run.status, 4);
    assert_eq!(run.error_code(), "auth_failed");
    assert!(w.server.received().is_empty());
}

/// §5.5's undo-send window: a delayed message is not eligible yet, and
/// cancelling it inside the window stops it entirely.
#[test]
fn a_delayed_send_can_be_cancelled_before_it_goes() {
    let w = World::new();
    let result = w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Wait",
        "--body",
        "Hello.",
        "--delay",
        "300",
    ]);
    let id = result["id"].as_i64().unwrap();
    assert!(result["send_after"].is_string());

    assert_eq!(w.flush()["attempted"], 0, "the window is not up yet");
    assert!(w.server.received().is_empty());

    w.json(&["outbox", "cancel", &id.to_string()]);
    assert_eq!(w.entry(id)["state"], "cancelled");
    assert_eq!(w.flush()["attempted"], 0);
    assert!(w.server.received().is_empty());
}

/// §6.5's approval queue. Under `MAILBOX_AGENT=1` a send is held, whatever the
/// config says, and only a human releasing it puts it on the wire.
#[test]
fn agent_mode_holds_sends_for_approval() {
    let w = World::new();
    let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args([
            "--json",
            "send",
            "--to",
            "alice@example.com",
            "--subject",
            "From an agent",
            "--body",
            "Hello.",
            "--yes",
        ])
        .env("MAILBOX_CONFIG", &w.config)
        .env("MAILBOX_STATE_DIR", &w.state)
        .env("MAILBOX_PASSWORD_TEST", "testpass")
        .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
        .env("MAILBOX_SEED", "l3")
        .env("MAILBOX_AGENT", "1")
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(0));

    let result: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(result["status"], "pending_approval");
    let id = result["id"].as_i64().unwrap();

    assert_eq!(w.flush()["attempted"], 0, "held mail must not be sent");
    assert!(w.server.received().is_empty());

    w.json(&["outbox", "approve", &id.to_string()]);
    assert_eq!(w.flush()["sent"], 1);
    assert_eq!(w.server.received().len(), 1);
}

/// §6.5's policy file. A denied recipient fails with `policy_denied`, cites the
/// rule, and — the part that matters — leaves nothing behind to be sent later.
#[test]
fn a_policy_denial_sends_nothing_and_names_the_rule() {
    let w = World::new();
    let policy_dir = w.config.parent().unwrap();
    std::fs::write(
        policy_dir.join("policy.toml"),
        "[send]\ndeny_recipients = [\"*@blocked.example\"]\nmax_recipients = 3\n",
    )
    .unwrap();

    let run = w.run(&[
        "--json",
        "send",
        "--to",
        "someone@blocked.example",
        "--subject",
        "Nope",
        "--body",
        "Hello.",
    ]);
    assert_eq!(run.status, 7, "§6.6 gives policy denial its own exit code");
    assert_eq!(run.error_code(), "policy_denied");
    assert!(
        run.stderr.contains("deny_recipients"),
        "the error must cite the rule: {}",
        run.stderr
    );

    assert!(w.outbox().is_empty(), "a denied send must not be queued");
    assert!(w.server.received().is_empty());
}

#[test]
fn a_policy_recipient_cap_is_enforced() {
    let w = World::new();
    std::fs::write(
        w.config.parent().unwrap().join("policy.toml"),
        "[send]\nmax_recipients = 2\n",
    )
    .unwrap();

    let run = w.run(&[
        "--json",
        "send",
        "--to",
        "a@example.com,b@example.com,c@example.com",
        "--subject",
        "Too many",
        "--body",
        "Hello.",
    ]);
    assert_eq!(run.status, 7);
    assert!(run.stderr.contains("max_recipients"));
    assert!(w.outbox().is_empty());
}

/// §6.5: `--dry-run` renders the exact bytes and exits 0 without touching the
/// network. Pointed at the L3 server, "without touching the network" is
/// checkable rather than asserted.
#[test]
fn dry_run_renders_bytes_and_sends_nothing() {
    let w = World::new();
    let result = w.json(&[
        "--dry-run",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Preview",
        "--body",
        "Hello.",
    ]);
    assert_eq!(result["status"], "dry_run");
    let raw = result["raw"].as_str().expect("the rendered bytes");
    assert!(raw.contains("Subject: Preview"));
    assert!(raw.contains("To: alice@example.com"));

    assert!(w.outbox().is_empty(), "a dry run must not queue anything");
    assert!(w.server.received().is_empty());
}

/// §13: `mailbox send --stdin` takes exact RFC5322 bytes, which is what makes
/// `sendmail_command = "mailbox send --stdin"` and `git send-email` work.
#[test]
fn send_stdin_accepts_a_whole_message() {
    use std::io::Write;

    let w = World::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mailbox"))
        .args(["--json", "send", "--stdin"])
        .env("MAILBOX_CONFIG", &w.config)
        .env("MAILBOX_STATE_DIR", &w.state)
        .env("MAILBOX_PASSWORD_TEST", "testpass")
        // The same pinned clock the rest of the world uses. Without it this
        // process stamps `send_after` from the real clock while the flush reads
        // the pinned one, and the message is not due until 2026.
        .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
        .env("MAILBOX_SEED", "l3")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            b"From: peter@example.com\r\n\
              To: alice@example.com\r\n\
              Subject: Piped\r\n\
              Message-ID: <piped@example.com>\r\n\
              \r\n\
              Sent through a pipe.\r\n",
        )
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    w.flush();
    let received = w.server.received();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].to, vec!["alice@example.com".to_string()]);
    assert_eq!(
        received[0].message_id().as_deref(),
        Some("<piped@example.com>"),
        "the caller's own Message-ID must survive"
    );
}

/// A slow server must hit `--timeout` rather than hanging, and the message must
/// survive to be retried.
#[test]
fn a_slow_server_times_out_without_losing_the_message() {
    let w = World::new();
    let (id, _) = w.queue("Slow");
    w.server
        .set_fault(Fault::Delay(Stage::MailFrom, Duration::from_secs(5)));

    let run = w.run(&["--json", "--timeout", "500", "outbox", "flush"]);
    // Either a clean deferral or a timeout error, but never a lost message.
    assert_ne!(w.entry(id)["state"], "sent");
    assert!(w.server.received().is_empty());
    assert!(
        run.status == 0 || run.status == 9 || run.status == 5,
        "unexpected exit {}: {}",
        run.status,
        run.stderr
    );
}

/// The outbox is durable: a queued message survives the process exiting, which
/// is the whole reason it is a table and not a field (§5.5).
#[test]
fn the_outbox_survives_a_restart() {
    let w = World::new();
    let (id, message_id) = w.queue("Durable");
    // Every `w.run` is a fresh process, so simply asking again *is* the restart.
    assert_eq!(w.entry(id)["state"], "queued");
    w.flush();
    assert_eq!(w.server.deliveries_of(&message_id), 1);
}

// ---- M7: the policy and approval rules, against the L3 server ----

/// The rate limit of §6.5, against the thing it exists for.
///
/// A limit that counted only *delivered* mail was no limit at all: a loop that
/// queues two hundred messages in a second has delivered none of them, so every
/// check saw zero and let the next one through. What has to be counted is mail
/// this account is committed to sending.
#[test]
fn the_rate_limit_stops_a_burst_before_any_of_it_is_sent() {
    let w = World::new();
    std::fs::write(
        w.config.parent().unwrap().join("policy.toml"),
        "[send]\nmax_per_hour = 2\n",
    )
    .unwrap();

    for i in 0..2 {
        let run = w.run(&[
            "--json",
            "send",
            "--to",
            "alice@example.com",
            "--subject",
            &format!("Within the limit {i}"),
            "--body",
            "Hello.",
        ]);
        assert_eq!(run.status, 0, "{}", run.stderr);
    }

    // Nothing has been flushed, so nothing has been *sent* — and the third
    // message must still be refused.
    let run = w.run(&[
        "--json",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Over the limit",
        "--body",
        "Hello.",
    ]);
    assert_eq!(run.status, 7, "§6.6's policy exit code: {}", run.stdout);
    assert_eq!(run.error_code(), "policy_denied");
    assert!(
        run.stderr.contains("max_per_hour"),
        "the error must cite the rule: {}",
        run.stderr
    );

    assert_eq!(
        w.outbox().len(),
        2,
        "the refused message must not be queued"
    );
    assert_eq!(w.flush()["sent"], 2);
    assert_eq!(
        w.server.received().len(),
        2,
        "and only two reached the server"
    );
}

/// A message that was cancelled was never sent, and must not go on holding a
/// slot in the hour: an agent that cancels its own draft would otherwise lock
/// itself out until the window rolled past.
#[test]
fn a_cancelled_message_gives_its_slot_back() {
    let w = World::new();
    let (id, _) = w.queue("Withdrawn");
    // Written after queuing: `queue` previews the message to recover its
    // Message-ID, and a preview is refused when a real send would be — see
    // `a_dry_run_is_refused_when_a_real_send_would_be`.
    std::fs::write(
        w.config.parent().unwrap().join("policy.toml"),
        "[send]\nmax_per_hour = 1\n",
    )
    .unwrap();
    w.json(&["outbox", "cancel", &id.to_string()]);

    let run = w.run(&[
        "--json",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "The real one",
        "--body",
        "Hello.",
    ]);
    assert_eq!(
        run.status, 0,
        "a cancelled message should not count against the limit: {}",
        run.stderr
    );
}

/// Retrying after a failure mid-send must not cost a second slot. The
/// alternative is an outbox that eats its own budget on a flaky link — the
/// worse the network, the tighter the limit becomes.
#[test]
fn a_retry_after_a_mid_send_failure_does_not_spend_the_limit_twice() {
    let w = World::new();
    std::fs::write(
        w.config.parent().unwrap().join("policy.toml"),
        "[send]\nmax_per_hour = 2\n",
    )
    .unwrap();

    let (id, message_id) = w.queue("Interrupted");
    w.server.set_fault(Fault::DropDuringData);
    assert_eq!(w.flush()["sent"], 0);
    assert_eq!(w.server.deliveries_of(&message_id), 0);

    w.server.set_fault(Fault::None);
    w.json(&["outbox", "retry", &id.to_string()]);
    assert_eq!(w.flush()["sent"], 1);
    assert_eq!(
        w.server.deliveries_of(&message_id),
        1,
        "exactly one copy after the retry"
    );

    // One message went out, and one slot was spent. The second is still there.
    let run = w.run(&[
        "--json",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "The second of two",
        "--body",
        "Hello.",
    ]);
    assert_eq!(
        run.status, 0,
        "the retry consumed a second slot: {}",
        run.stderr
    );
}

/// A message held for a human counts: it is going to be sent, and pretending
/// otherwise would let an agent queue an unbounded backlog for someone to find.
#[test]
fn mail_waiting_for_approval_counts_against_the_limit() {
    let w = World::new();
    std::fs::write(
        w.config.parent().unwrap().join("policy.toml"),
        "[send]\nmax_per_hour = 1\n",
    )
    .unwrap();

    let held = w.agent(&[
        "--json",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "For approval",
        "--body",
        "Hello.",
        "--yes",
    ]);
    assert_eq!(held.json()["status"], "pending_approval");

    let second = w.agent(&[
        "--json",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "And another",
        "--body",
        "Hello.",
        "--yes",
    ]);
    assert_eq!(second.status, 7, "{}", second.stdout);
    assert!(second.stderr.contains("max_per_hour"), "{}", second.stderr);
}

/// The approval queue survives a failure on the way out: releasing a message
/// and having the server drop the connection must leave it queued and
/// retryable, not silently back in `pending_approval` (where the next flush
/// would ignore it) and not marked sent (§5.5).
#[test]
fn an_approved_message_that_fails_mid_send_stays_queued() {
    let w = World::new();
    let approved = w.agent(&[
        "--json",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Released",
        "--body",
        "Hello.",
        "--yes",
    ]);
    let id = approved.json()["id"].as_i64().expect("an outbox id");
    w.json(&["outbox", "approve", &id.to_string()]);

    w.server.set_fault(Fault::DropDuringData);
    assert_eq!(w.flush()["sent"], 0);
    let entry = w.entry(id);
    assert_eq!(
        entry["state"], "queued",
        "a failed delivery must not walk the message back to pending_approval: {entry}"
    );

    w.server.set_fault(Fault::None);
    w.json(&["outbox", "retry", &id.to_string()]);
    assert_eq!(w.flush()["sent"], 1);
    assert_eq!(w.entry(id)["state"], "sent");
}

/// And the preview tells the truth about the limit too.
///
/// A `--dry-run` consumes nothing and sends nothing, so it could have been
/// exempted — but a preview that succeeds where the send it previews would be
/// refused is a preview that lies, which is the one thing it must not do.
#[test]
fn a_dry_run_is_refused_when_a_real_send_would_be() {
    let w = World::new();
    std::fs::write(
        w.config.parent().unwrap().join("policy.toml"),
        "[send]\nmax_per_hour = 1\n",
    )
    .unwrap();
    w.json(&[
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "The one allowed",
        "--body",
        "Hello.",
    ]);

    let run = w.run(&[
        "--json",
        "--dry-run",
        "send",
        "--to",
        "alice@example.com",
        "--subject",
        "Preview",
        "--body",
        "Hello.",
    ]);
    assert_eq!(run.status, 7, "{}", run.stdout);
    assert!(run.stderr.contains("max_per_hour"), "{}", run.stderr);
    // And it still consumed nothing.
    assert_eq!(w.outbox().len(), 1);
}

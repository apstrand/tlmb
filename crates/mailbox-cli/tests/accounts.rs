//! `mailbox accounts add|edit|remove|login` through the real binary (§9).
//!
//! The point of these, over the L0 tests of the same rules, is the parts only
//! the binary has: the config file it chooses, the password it reads from
//! stdin and must not print, the exit codes of §6.6, and the JSON contract.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct World {
    dir: PathBuf,
    config: PathBuf,
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
        let dir = std::env::temp_dir().join(format!("mailbox-accts-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        World {
            config: dir.join("config.toml"),
            dir,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", self.dir.join("state"))
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            .env_remove("MAILBOX_AGENT");
        cmd
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = self.command(args).output().expect("run");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Runs with something on stdin, for `--password-stdin`.
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Run {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("wait");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap_or_default()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- adding ----

#[test]
fn adding_a_known_provider_takes_only_an_address() {
    let w = World::new();
    let run = w.run(&["--json", "accounts", "add", "peter@gmail.com"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let doc = run.json();
    assert_eq!(doc["id"], "peter");
    assert_eq!(doc["discovered"], true);
    assert!(w.config_text().contains("imap.gmail.com"));

    // And it is visible to every other command straight away.
    let listed = w.run(&["--json", "accounts", "list"]).json();
    assert_eq!(listed["accounts"][0]["id"], "peter");
}

#[test]
fn a_domain_nothing_is_known_about_asks_for_a_host() {
    let w = World::new();
    let run = w.run(&["--json", "accounts", "add", "peter@example.org"]);
    assert_eq!(run.status, 2, "§6.6's usage code: {}", run.stdout);
    assert_eq!(run.error_code(), "usage");
    assert!(run.stderr.contains("imap-host"), "{}", run.stderr);

    let run = w.run(&[
        "--json",
        "accounts",
        "add",
        "peter@example.org",
        "--imap-host",
        "imap.example.org",
    ]);
    assert_eq!(run.status, 0, "{}", run.stderr);
}

#[test]
fn a_password_arrives_on_stdin_and_is_not_written_anywhere() {
    let w = World::new();
    let run = w.run_with_stdin(
        &[
            "--json",
            "accounts",
            "add",
            "peter@gmail.com",
            "--password-stdin",
        ],
        "hunter2\n",
    );
    assert_eq!(run.status, 0, "{}", run.stderr);

    assert!(
        !w.config_text().contains("hunter2"),
        "§9: never in the config:\n{}",
        w.config_text()
    );
    assert!(
        !run.stdout.contains("hunter2") && !run.stderr.contains("hunter2"),
        "and never echoed back"
    );
}

#[test]
fn an_empty_stdin_is_an_error_rather_than_an_empty_password() {
    let w = World::new();
    let run = w.run_with_stdin(
        &[
            "--json",
            "accounts",
            "add",
            "peter@gmail.com",
            "--password-stdin",
        ],
        "",
    );
    assert_eq!(run.status, 2, "{}", run.stdout);
    assert!(run.stderr.contains("stdin"), "{}", run.stderr);
}

#[test]
fn a_dry_run_adds_nothing() {
    let w = World::new();
    let run = w.run(&["--json", "--dry-run", "accounts", "add", "peter@gmail.com"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(run.json()["would_add"], "peter@gmail.com");
    assert!(
        w.config_text().is_empty(),
        "a dry run must not write the config"
    );
}

// ---- §6.4: agent mode ----

#[test]
fn agent_mode_will_not_add_an_account_without_yes() {
    let w = World::new();
    let out = w
        .command(&["--json", "accounts", "add", "peter@gmail.com"])
        .env("MAILBOX_AGENT", "1")
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(8),
        "§6.6's needs_confirmation: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(w.config_text().is_empty());

    let out = w
        .command(&["--json", "accounts", "add", "peter@gmail.com", "--yes"])
        .env("MAILBOX_AGENT", "1")
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(0));
}

// ---- editing and removing ----

#[test]
fn editing_changes_a_setting_in_place() {
    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com"]);

    let run = w.run(&[
        "--json",
        "accounts",
        "edit",
        "peter",
        "--imap-user",
        "peter.strand",
    ]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(w.config_text().contains("peter.strand"));

    let listed = w.run(&["--json", "accounts", "list"]).json();
    assert_eq!(listed["accounts"][0]["login_user"], "peter.strand");
}

#[test]
fn removing_reports_the_store_it_did_not_delete() {
    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com"]);

    let run = w.run(&["--json", "accounts", "remove", "peter"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    let doc = run.json();
    assert_eq!(doc["removed"], "peter");
    assert_eq!(
        doc["store_removed"], false,
        "a mailbox's cache is not something to lose to a mistyped id"
    );

    let listed = w.run(&["--json", "accounts", "list"]);
    assert_ne!(listed.status, 0, "there are no accounts left");
}

#[test]
fn removing_an_account_that_is_not_there_is_not_found() {
    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com"]);
    let run = w.run(&["--json", "accounts", "remove", "nope"]);
    assert_eq!(run.status, 3, "§6.6's not_found: {}", run.stdout);
    assert_eq!(run.error_code(), "not_found");
}

// ---- OAuth2 ----

#[test]
fn an_oauth_account_without_a_client_id_says_where_to_get_one() {
    let w = World::new();
    let run = w.run(&[
        "--json",
        "accounts",
        "add",
        "peter@gmail.com",
        "--oauth",
        "google",
    ]);
    assert_eq!(run.status, 2, "{}", run.stdout);
    assert!(run.stderr.contains("oauth_client_id"), "{}", run.stderr);
}

#[test]
fn logging_in_without_a_client_id_explains_why_none_ships() {
    let w = World::new();
    // Written by hand, since `add` refuses to create this state.
    std::fs::write(
        &w.config,
        "[accounts.work]\naddress = \"peter@example.com\"\n\
         imap_host = \"imap.example.com\"\noauth_provider = \"google\"\n",
    )
    .unwrap();

    let run = w.run(&["--json", "accounts", "login", "work"]);
    assert_eq!(run.status, 2, "{}", run.stdout);
    let message = run.stderr.to_lowercase();
    assert!(message.contains("client id"), "{}", run.stderr);
    assert!(
        message.contains("console.cloud.google.com"),
        "and it should say where to register one: {}",
        run.stderr
    );
}

#[test]
fn an_oauth_account_reports_its_missing_token_rather_than_a_missing_password() {
    let w = World::new();
    w.run(&[
        "accounts",
        "add",
        "peter@gmail.com",
        "--oauth",
        "google",
        "--oauth-client-id",
        "abc.apps.googleusercontent.com",
    ]);

    let run = w.run(&["--json", "accounts", "test", "peter"]);
    assert_ne!(run.status, 0);
    assert!(
        run.stderr.contains("accounts login"),
        "the answer is to log in, and the error should say so: {}",
        run.stderr
    );
}

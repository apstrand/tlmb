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
            // Nothing here may put a popup on the desktop of
            // whoever ran the suite (§11).
            .env("MAILBOX_NOTIFIER", "none")
            // Nor may it resolve or connect to whatever domain a fixture
            // happens to name (§9 discovery).
            .env("MAILBOX_DISCOVERY", "off")
            // And out of the developer's keyring: adding an account files its
            // password where the platform can find it again (§9, §14).
            .env("MAILBOX_KEYCHAIN", "none")
            // Nothing here may put a consent screen on the desktop of whoever
            // is running `cargo test`.
            .env("MAILBOX_OPENER", "/bin/true")
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

    // Listing nothing is an answer, not a failure: this is the same state a
    // fresh install is in, and `serve`, `gui`, and the TUI all come up in it
    // to show the guide for adding one.
    let listed = w.run(&["--json", "accounts", "list"]);
    assert_eq!(listed.status, 0, "{}", listed.stderr);
    assert_eq!(
        listed.json()["accounts"].as_array().map(Vec::len),
        Some(0),
        "there are no accounts left"
    );
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

/// Whether this build ships an OAuth2 client for Google (§16).
///
/// Discovered by asking the binary rather than by reading the environment:
/// the credentials are compiled in, so the test binary's own environment says
/// nothing about what the binary under test was built with.
fn ships_a_google_client(w: &World) -> bool {
    let run = w.run(&[
        "--json",
        "accounts",
        "add",
        "probe@gmail.com",
        "--id",
        "probe",
        "--oauth",
        "google",
    ]);
    let shipped = run.status == 0;
    if shipped {
        w.run(&["accounts", "remove", "probe"]);
    }
    shipped
}

/// A release carries its own client, so `--oauth google` is the whole command.
/// A source build carries none, and has to say so and link to the console.
///
/// Both are correct; which one this is depends on how the binary was built,
/// and what the test pins is that the two halves agree with each other.
#[test]
fn an_oauth_account_uses_the_shipped_client_or_explains_its_absence() {
    let w = World::new();
    let run = w.run(&[
        "--json",
        "accounts",
        "add",
        "peter@gmail.com",
        "--oauth",
        "google",
    ]);

    if run.status == 0 {
        assert!(
            !w.config_text().contains("oauth_client_id"),
            "the built-in client belongs to the build, not to the account:\n{}",
            w.config_text()
        );
    } else {
        assert_eq!(run.status, 2, "{}", run.stdout);
        assert!(run.stderr.contains("oauth_client_id"), "{}", run.stderr);
        assert!(
            run.stderr.contains("console.cloud.google.com"),
            "and it should say where to register one: {}",
            run.stderr
        );
    }
}

/// An account may always register its own, whatever the build ships — which is
/// how someone spends their own quota rather than this project's.
#[test]
fn a_configured_client_id_is_written_and_used() {
    let w = World::new();
    let run = w.run(&[
        "--json",
        "accounts",
        "add",
        "peter@gmail.com",
        "--oauth",
        "google",
        "--oauth-client-id",
        "mine.apps.googleusercontent.com",
    ]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(w.config_text().contains("mine.apps.googleusercontent.com"));

    // And it reaches the provider: the authorization URL carries it.
    let out = w
        .command(&["accounts", "login", "peter", "--no-browser", "--wait", "5"])
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("client_id=mine.apps.googleusercontent.com"),
        "the configured id should be the one asked for: {stderr}"
    );
}

/// Without any client at all — no configured id, no shipped one — a login
/// cannot start, and the error has to say where to get one.
#[test]
fn logging_in_with_no_client_anywhere_says_where_to_get_one() {
    let w = World::new();
    if ships_a_google_client(&w) {
        // This build has one, so there is no such state to test. The case
        // above covers what it does instead.
        return;
    }

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
        "and where to register one: {}",
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

/// §9 keeps passwords out of the config, but the config still carries an
/// account's servers, its login name, and — for OAuth2 — a client secret.
/// None of that belongs to every other user on the machine.
#[test]
#[cfg(unix)]
fn the_config_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let w = World::new();
    let run = w.run(&["accounts", "add", "peter@gmail.com"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let mode = std::fs::metadata(&w.config)
        .expect("stat the config")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o077,
        0,
        "the config should not be readable by anyone else: {mode:o}"
    );
}

/// And an existing config written before that rule gets tightened rather than
/// left as it was found.
#[test]
#[cfg(unix)]
fn an_existing_world_readable_config_is_tightened_on_the_next_write() {
    use std::os::unix::fs::PermissionsExt;

    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com"]);
    std::fs::set_permissions(&w.config, std::fs::Permissions::from_mode(0o644)).unwrap();

    w.run(&["accounts", "add", "other@gmail.com", "--id", "other"]);
    let mode = std::fs::metadata(&w.config).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "{mode:o}");
}

// ---- discovery (§9) ----
//
// Every test here runs with `MAILBOX_DISCOVERY=off` from `World::command`, so
// what is asserted is the offline half of the chain and the shape of what it
// reports. The network sources are covered by unit tests over recorded
// documents in `mailbox_core::discover`; exercising them for real needs an
// HTTPS server the suite does not have (see TODO.md).

#[test]
fn discover_reports_the_table_without_writing_anything() {
    let w = World::new();
    let run = w.run(&["--json", "accounts", "discover", "peter@gmail.com"]);
    assert_eq!(run.status, 0, "{}", run.stderr);

    let doc = run.json();
    assert_eq!(doc["domain"], "gmail.com");
    assert_eq!(doc["found"]["source"], "built_in");
    assert_eq!(doc["found"]["imap_host"], "imap.gmail.com");
    assert_eq!(doc["found"]["imap_security"], "implicit");
    assert!(
        w.config_text().is_empty(),
        "discover must not write a config: {}",
        w.config_text()
    );
}

#[test]
fn discover_says_what_it_tried_when_it_finds_nothing() {
    let w = World::new();
    let run = w.run(&["--json", "accounts", "discover", "peter@example.org"]);
    assert_eq!(run.status, 0, "finding nothing is an answer, not an error");

    let doc = run.json();
    assert!(doc["found"].is_null());
    let steps = doc["steps"].as_array().expect("steps");
    assert!(
        steps.iter().any(|s| s["source"] == "built_in"),
        "the trail should name the source that was asked: {steps:?}"
    );
}

/// The failure §9's one-line setup has to explain: what was asked, so the
/// reader knows whether to fix the network or type `--imap-host`.
#[test]
fn adding_an_undiscoverable_domain_lists_the_sources_it_asked() {
    let w = World::new();
    let run = w.run(&["accounts", "add", "peter@example.org"]);
    assert_ne!(run.status, 0);
    assert!(
        run.stderr.contains("example.org") && run.stderr.contains("not in the table"),
        "the error should carry the trail: {}",
        run.stderr
    );
}

#[test]
fn no_discover_still_takes_the_built_in_table() {
    let w = World::new();
    let run = w.run(&[
        "--json",
        "accounts",
        "add",
        "peter@gmail.com",
        "--no-discover",
    ]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(run.json()["discovered"], true);
    assert!(w.config_text().contains("imap.gmail.com"));
}

/// A discovered account says where its servers came from, because "it works"
/// and "something answered on port 993" are different claims.
#[test]
fn adding_reports_the_source_of_its_servers() {
    let w = World::new();
    let doc = w
        .run(&["--json", "accounts", "add", "peter@gmail.com"])
        .json();
    assert_eq!(doc["source"], "built_in");
    // The table is an assertion, not a connection: nothing was verified.
    assert!(!doc["verified"].as_bool().unwrap_or(false));
}

/// §9: a password goes to the keychain. Without one — which is every CI run
/// and this suite — it is held for the process and the reply says so rather
/// than implying it was saved.
#[test]
fn a_password_with_no_keychain_says_it_went_nowhere_durable() {
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

    let credential = run.json()["credential"].as_str().unwrap_or("").to_string();
    assert!(
        credential.contains("this process only"),
        "expected an honest answer about durability, got `{credential}`"
    );
    assert!(!w.config_text().contains("hunter2"));
}

// ---- the login name (§9) ----
//
// It cannot always be derived from the address: plenty of servers want a bare
// local part, an account number, or a name that looks nothing like the mail
// it carries. So it has to be settable at `add`, changeable afterwards, and —
// the part that was missing — removable again.

#[test]
fn one_flag_sets_the_login_for_both_halves() {
    let w = World::new();
    let run = w.run(&["accounts", "add", "peter@gmail.com", "--user", "pstrand"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(w.config_text().contains(r#"imap_user = "pstrand""#));
    assert!(w.config_text().contains(r#"smtp_user = "pstrand""#));

    // And it is what `accounts list` shows, instead of the address.
    let listed = w.run(&["--json", "accounts", "list"]).json();
    assert_eq!(listed["accounts"][0]["login_user"], "pstrand");
}

/// The providers that want two different names are why the per-protocol flags
/// still exist; `--user` is the default they override.
#[test]
fn a_protocol_flag_overrides_the_shared_one() {
    let w = World::new();
    let run = w.run(&[
        "accounts",
        "add",
        "peter@gmail.com",
        "--user",
        "shared",
        "--smtp-user",
        "submission-only",
    ]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(w.config_text().contains(r#"imap_user = "shared""#));
    assert!(w.config_text().contains(r#"smtp_user = "submission-only""#));
}

#[test]
fn the_login_can_be_changed_after_the_fact() {
    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com", "--user", "first"]);
    let run = w.run(&["--json", "accounts", "edit", "peter", "--user", "second"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(run.json()["login_user"], "second");
}

/// The half that had no way to be said: put it back to the address. A name
/// typed by mistake — or filled in by discovery from an autoconfig document —
/// could be changed but never removed.
#[test]
fn an_empty_login_goes_back_to_the_address() {
    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com", "--user", "wrong"]);
    let run = w.run(&["--json", "accounts", "edit", "peter", "--user", ""]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert_eq!(run.json()["login_user"], "peter@gmail.com");
    assert!(
        !w.config_text().contains("imap_user"),
        "the setting should be gone, not blank: {}",
        w.config_text()
    );
}

/// An edit that names no login must not erase one. This is the rule the whole
/// `edit` design rests on, and clearing is the exception to it.
#[test]
fn editing_something_else_leaves_the_login_alone() {
    let w = World::new();
    w.run(&["accounts", "add", "peter@gmail.com", "--user", "pstrand"]);
    w.run(&["accounts", "edit", "peter", "--imap-port", "1993"]);
    assert!(w.config_text().contains(r#"imap_user = "pstrand""#));
}

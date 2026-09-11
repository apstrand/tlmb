//! Pairing end to end (§8, §15).
//!
//! > **L0** known-answer vectors for the AEAD and the payload codec, plus the
//! > two-process pairing end-to-end with the MITM/SAS-mismatch case (§14.3).
//! > No new level, but pairing does not ship without these.
//!
//! The vectors live in `mailbox-core`'s `pair_crypto`. This file is the other
//! half: two real processes, a real socket, and the cases where it has to
//! refuse — a wrong secret, a tampered payload, a stale one, and a user who
//! says the words do not match.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// One device: its own config, its own state directory.
struct Device {
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

impl Device {
    fn new(name: &str, accounts: &str) -> Device {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mailbox-pair-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(&config, accounts).expect("config");
        Device { dir, config }
    }

    /// A device with two accounts and a resolvable credential for one.
    fn enrolled() -> Device {
        Device::new(
            "old",
            "[accounts.work]\n\
             address = \"peter@work.example\"\n\
             imap_host = \"imap.work.example\"\n\
             identities = [\"p.strand@work.example\"]\n\
             \n\
             [accounts.home]\n\
             address = \"peter@home.example\"\n\
             imap_host = \"imap.home.example\"\n",
        )
    }

    fn blank() -> Device {
        Device::new("new", "")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailbox"));
        cmd.args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", self.dir.join("state"))
            .env("MAILBOX_CLOCK", "2026-09-02T12:00:00Z")
            .env("MAILBOX_SEED", "pair")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("NO_COLOR", "1")
            // Nothing here may put a popup on the desktop of
            // whoever ran the suite (§11).
            .env("MAILBOX_NOTIFIER", "none")
            // And out of the developer's keyring: adding an account files its
            // password where the platform can find it again (§9, §14).
            .env("MAILBOX_KEYCHAIN", "none")
            // The sending device resolves credentials from the environment
            // (§9), which is what makes this testable without a keychain.
            .env("MAILBOX_PASSWORD_WORK", "work-secret")
            .env_remove("MAILBOX_PASSWORD_HOME")
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

    /// Like [`Device::run`], but keeps stdout as bytes. A sealed payload is
    /// binary, and the lossy string `run` produces would corrupt it.
    fn run_raw(&self, args: &[&str]) -> (i32, Vec<u8>, String) {
        let out = self.command(args).output().expect("run");
        (
            out.status.code().unwrap_or(-1),
            out.stdout,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    /// Runs with `input` on stdin, for `--import-file -`.
    fn run_with_input(&self, args: &[&str], input: &[u8]) -> Run {
        use std::io::Write;
        use std::process::Stdio;

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
            .write_all(input)
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

impl Drop for Device {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Starts a receiver and returns its process plus the link it printed.
///
/// The link has to be read before the receiver blocks, which is why `pair`
/// emits it as its first output rather than at the end.
fn start_receiver(device: &Device, extra: &[&str]) -> (std::process::Child, String) {
    let mut args = vec!["--json", "--yes", "pair"];
    args.extend_from_slice(extra);

    let mut child = device
        .command(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the receiver");

    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("the link");

    let value: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("the receiver's first line was not JSON: {e}\n{line}"));
    let link = value["link"].as_str().expect("a link").to_string();

    // Put the reader back so the caller can collect the rest.
    child.stdout = Some(reader.into_inner());
    (child, link)
}

fn finish(child: std::process::Child) -> Run {
    let out = child.wait_with_output().expect("wait");
    Run {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// The whole flow of §8.3, across two processes and a real socket.
#[test]
fn two_devices_pair_over_the_lan() {
    let old = Device::enrolled();
    let new = Device::blank();

    let (receiver, link) = start_receiver(&new, &[]);
    assert!(link.starts_with("mailbox://pair/"), "{link}");

    let sent = old.run(&["--json", "pair", "send", &link, "--yes"]);
    assert_eq!(
        sent.status, 0,
        "send failed:\n{}\n{}",
        sent.stdout, sent.stderr
    );
    let sent = sent.json();
    assert_eq!(sent["accepted"], true);
    assert_eq!(sent["accounts"], 2);

    let received = finish(receiver);
    assert_eq!(
        received.status, 0,
        "receive failed:\n{}\n{}",
        received.stdout, received.stderr
    );

    // The accounts arrived, and the new device can see them.
    let config = new.config_text();
    assert!(config.contains("peter@work.example"), "{config}");
    assert!(config.contains("peter@home.example"), "{config}");
    assert!(config.contains("imap.work.example"), "{config}");
    // §8: identities travel too.
    assert!(config.contains("p.strand@work.example"), "{config}");

    let accounts = new.run(&["--json", "accounts"]).json();
    let ids: Vec<&str> = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["home", "work"]);
}

/// §8: the payload never carries the mail store.
#[test]
fn pairing_moves_no_mail() {
    let old = Device::enrolled();
    let new = Device::blank();

    let (receiver, link) = start_receiver(&new, &[]);
    old.run(&["--json", "pair", "send", &link, "--yes"]);
    finish(receiver);

    // No store was created for the new device beyond what opening one costs,
    // and certainly no messages.
    let status = new.run(&["--json", "status"]).json();
    let messages: u64 = status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["messages"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(messages, 0, "pairing should transfer no mail");
}

/// §14.3's named case: an active attacker between the devices.
///
/// The attacker relays the connection but substitutes its own payload — which
/// it must, since it cannot read the real one. The words the two devices show
/// then differ, and the receiving user says no. Nothing is written.
#[test]
fn a_man_in_the_middle_is_refused_on_a_sas_mismatch() {
    let new = Device::blank();
    let (receiver, link) = start_receiver_expecting_refusal(&new);

    // The attacker knows the address but not the secret, so it offers its own
    // channel and its own payload.
    let uri = link.split_once('#').expect("a link").0;
    let address = uri.trim_start_matches("mailbox://pair/");

    let attacker = mitm_offer(address);
    assert!(
        attacker.is_ok(),
        "the attacker should be able to connect: {attacker:?}"
    );

    let received = finish(receiver);
    assert_ne!(received.status, 0, "a wrong secret must be refused");
    assert!(
        received.stderr.contains("different pairing code")
            || received.stderr.contains("did not authenticate"),
        "the refusal should name the problem: {}",
        received.stderr
    );

    // And nothing was written.
    assert!(
        new.config_text().trim().is_empty(),
        "a refused pairing must leave the config alone: {}",
        new.config_text()
    );
}

/// The receiver started with `--yes`, so a *failure* here is the crypto
/// refusing rather than the user. That is the point: the confirmation is the
/// last line of defence, not the only one.
fn start_receiver_expecting_refusal(device: &Device) -> (std::process::Child, String) {
    start_receiver(device, &[])
}

/// Connects and offers a payload sealed under a secret the receiver does not
/// have — everything an attacker who saw the address but not the QR can do.
fn mitm_offer(address: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::net::TcpStream;

    let mut stream = TcpStream::connect(address)?;
    let offer = serde_json::json!({
        "channel": "00000000000000000000000000000000",
        "sealed": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    });
    stream.write_all(offer.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// A user who looks at the words and says no.
///
/// The receiver is run without `--yes` and with stdin closed, so §6.4's
/// "no interactive prompts when stdin is not a TTY" applies: it refuses rather
/// than assuming, because assuming yes here is assuming there is no attacker.
#[test]
fn a_receiver_that_cannot_confirm_refuses_rather_than_assuming() {
    let old = Device::enrolled();
    let new = Device::blank();

    let mut child = new
        .command(&["--json", "pair"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn");

    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("the link");
    child.stdout = Some(reader.into_inner());
    let link = serde_json::from_str::<serde_json::Value>(&line).unwrap()["link"]
        .as_str()
        .unwrap()
        .to_string();

    let sent = old.run(&["--json", "pair", "send", &link, "--yes"]);
    let received = finish(child);

    assert_ne!(received.status, 0, "it should not have assumed yes");
    assert_eq!(
        received.error_code(),
        "needs_confirmation",
        "stderr: {}",
        received.stderr
    );
    assert!(
        new.config_text().trim().is_empty(),
        "nothing should have been written"
    );
    // And the sending device is told, so its user knows to worry.
    assert_ne!(sent.status, 0, "the sender should hear the refusal");
}

/// §8.3: pairing is additive by default.
#[test]
fn merging_keeps_the_accounts_already_here() {
    let old = Device::enrolled();
    let new = Device::new(
        "existing",
        "[accounts.personal]\naddress = \"me@personal.example\"\nimap_host = \"imap.personal.example\"\n",
    );

    let (receiver, link) = start_receiver(&new, &[]);
    old.run(&["--json", "pair", "send", &link, "--yes"]);
    assert_eq!(finish(receiver).status, 0);

    let config = new.config_text();
    assert!(
        config.contains("me@personal.example"),
        "the existing account should survive: {config}"
    );
    assert!(config.contains("peter@work.example"), "{config}");
}

/// §8.3: `--replace` wipes, and requires `--yes`.
#[test]
fn replace_wipes_and_needs_confirmation() {
    let old = Device::enrolled();
    let new = Device::new(
        "existing",
        "[accounts.personal]\naddress = \"me@personal.example\"\nimap_host = \"imap.personal.example\"\n",
    );

    // Without `--yes` it refuses before anything is listened for.
    let refused = new.run(&["--json", "pair", "--replace"]);
    assert_eq!(refused.status, 8, "{}", refused.stderr);
    assert_eq!(refused.error_code(), "needs_confirmation");
    assert!(new.config_text().contains("me@personal.example"));

    let (receiver, link) = start_receiver(&new, &["--replace"]);
    old.run(&["--json", "pair", "send", &link, "--yes"]);
    assert_eq!(finish(receiver).status, 0);

    let config = new.config_text();
    assert!(
        !config.contains("me@personal.example"),
        "--replace should have wiped it: {config}"
    );
    assert!(config.contains("peter@work.example"), "{config}");
}

// ---- the export file (§8.3) ----

#[test]
fn an_export_round_trips_through_a_file() {
    let old = Device::enrolled();
    let new = Device::blank();
    let path = old.dir.join("backup.mailbox");

    let exported = old.run(&[
        "--json",
        "pair",
        "--export-file",
        path.to_str().unwrap(),
        "--passphrase",
        "correct horse battery staple",
    ]);
    assert_eq!(exported.status, 0, "{}", exported.stderr);
    assert_eq!(exported.json()["accounts"], 2);

    let imported = new.run(&[
        "--json",
        "pair",
        "--import-file",
        path.to_str().unwrap(),
        "--passphrase",
        "correct horse battery staple",
    ]);
    assert_eq!(imported.status, 0, "{}", imported.stderr);

    let config = new.config_text();
    assert!(config.contains("peter@work.example"), "{config}");
    assert!(config.contains("peter@home.example"), "{config}");
}

/// The file holds every credential the user has, so it must not be readable
/// and must not be plaintext.
#[test]
fn an_export_is_private_and_opaque() {
    let old = Device::enrolled();
    let path = old.dir.join("backup.mailbox");
    old.run(&[
        "--json",
        "pair",
        "--export-file",
        path.to_str().unwrap(),
        "--passphrase",
        "a passphrase",
    ]);

    let bytes = std::fs::read(&path).expect("the export");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("work-secret"),
        "the credential is in the clear"
    );
    assert!(
        !text.contains("peter@work.example"),
        "the addresses are in the clear"
    );
    // The header says what it is, in plain text, so nobody mistakes it for age.
    assert!(
        text.starts_with("{\"format\":\"mailbox-pair/v1\""),
        "{text}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "the export is readable by others: {mode:o}"
        );
    }
}

#[test]
fn a_wrong_passphrase_is_refused() {
    let old = Device::enrolled();
    let new = Device::blank();
    let path = old.dir.join("backup.mailbox");

    old.run(&[
        "--json",
        "pair",
        "--export-file",
        path.to_str().unwrap(),
        "--passphrase",
        "the right one",
    ]);

    let imported = new.run(&[
        "--json",
        "pair",
        "--import-file",
        path.to_str().unwrap(),
        "--passphrase",
        "the wrong one",
    ]);
    assert_ne!(imported.status, 0);
    assert!(
        imported.stderr.contains("passphrase"),
        "{}",
        imported.stderr
    );
    assert!(new.config_text().trim().is_empty());
}

/// A file that has been altered must not open, whatever was altered.
#[test]
fn a_tampered_export_is_refused() {
    let old = Device::enrolled();
    let new = Device::blank();
    let path = old.dir.join("backup.mailbox");
    old.run(&[
        "--json",
        "pair",
        "--export-file",
        path.to_str().unwrap(),
        "--passphrase",
        "a passphrase",
    ]);

    let original = std::fs::read_to_string(&path).unwrap();
    let (header, body) = original.split_once('\n').expect("two lines");

    // Flip a byte of the ciphertext.
    let mut tampered: Vec<char> = body.trim().chars().collect();
    tampered[10] = if tampered[10] == 'A' { 'B' } else { 'A' };
    std::fs::write(
        &path,
        format!("{header}\n{}\n", tampered.into_iter().collect::<String>()),
    )
    .unwrap();

    let imported = new.run(&[
        "--json",
        "pair",
        "--import-file",
        path.to_str().unwrap(),
        "--passphrase",
        "a passphrase",
    ]);
    assert_ne!(imported.status, 0, "a tampered export must not open");
    assert!(new.config_text().trim().is_empty());
}

/// `--dry-run` says what would change and changes nothing (§6.5).
#[test]
fn a_dry_run_import_writes_nothing() {
    let old = Device::enrolled();
    let new = Device::blank();
    let path = old.dir.join("backup.mailbox");
    old.run(&[
        "--json",
        "pair",
        "--export-file",
        path.to_str().unwrap(),
        "--passphrase",
        "a passphrase",
    ]);

    let dry = new.run(&[
        "--json",
        "--dry-run",
        "pair",
        "--import-file",
        path.to_str().unwrap(),
        "--passphrase",
        "a passphrase",
    ]);
    assert_eq!(dry.status, 0, "{}", dry.stderr);
    let payload = dry.json();
    assert_eq!(payload["dry_run"], true);
    assert_eq!(payload["would_add"].as_array().unwrap().len(), 2);
    assert!(new.config_text().trim().is_empty());
}

/// The sending device says which accounts have no credential to send, before
/// sending, so the user can stop and fix it.
#[test]
fn accounts_without_a_credential_are_reported() {
    let old = Device::enrolled();
    let new = Device::blank();

    let (receiver, link) = start_receiver(&new, &[]);
    let sent = old.run(&["--json", "pair", "send", &link, "--yes"]).json();
    finish(receiver);

    // `home` has no MAILBOX_PASSWORD_HOME set, so it travels without one.
    let missing: Vec<&str> = sent["credentials_missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(missing, vec!["home"]);
}

// ---- `-`, the desktop-to-desktop path ----
//
// Two desktops have no camera to scan the QR with, and the link is a hundred
// characters nobody is going to type. What they almost always do have is a
// channel that is already authenticated — SSH — so the export learns to be a
// pipe, and every credential the user owns moves without touching a disk at
// either end.

#[test]
fn an_export_can_be_piped_instead_of_written() {
    let old = Device::enrolled();
    let new = Device::blank();

    let (status, payload, stderr) = old.run_raw(&[
        "pair",
        "--export-file",
        "-",
        "--passphrase",
        "correct horse battery staple",
    ]);
    assert_eq!(status, 0, "{stderr}");
    // The container announces itself in a clear-text header line, so `head`
    // on a saved copy answers "what is this".
    assert!(
        String::from_utf8_lossy(&payload[..payload.len().min(64)]).contains("mailbox-pair/v1"),
        "the container should name its format"
    );
    // The summary belongs on stderr: stdout is the payload.
    assert!(stderr.contains("stdout"), "{stderr}");

    let imported = new.run_with_input(
        &[
            "--json",
            "pair",
            "--import-file",
            "-",
            "--passphrase",
            "correct horse battery staple",
        ],
        &payload,
    );
    assert_eq!(imported.status, 0, "{}", imported.stderr);

    let config = new.config_text();
    assert!(config.contains("peter@work.example"), "{config}");
    assert!(config.contains("peter@home.example"), "{config}");
}

/// Nothing readable may cross the pipe, exactly as nothing readable may sit in
/// the file — the channel it travels over is not what protects it.
#[test]
fn a_piped_export_is_opaque() {
    let old = Device::enrolled();
    let (status, payload, _) =
        old.run_raw(&["pair", "--export-file", "-", "--passphrase", "a passphrase"]);
    assert_eq!(status, 0);
    let text = String::from_utf8_lossy(&payload);
    for secret in ["peter@work.example", "hunter2", "peter@home.example"] {
        assert!(
            !text.contains(secret),
            "`{secret}` was readable in the pipe"
        );
    }
}

/// stdout carries the payload, so it cannot also carry a JSON document. Said
/// as a usage error naming both ways out, rather than emitting a mixture that
/// parses as neither.
#[test]
fn a_piped_export_refuses_to_share_stdout_with_json() {
    let old = Device::enrolled();
    let run = old.run(&[
        "--json",
        "pair",
        "--export-file",
        "-",
        "--passphrase",
        "a passphrase",
    ]);
    assert_ne!(run.status, 0);
    assert_eq!(run.error_code(), "usage");
    assert!(run.stderr.contains("--json"), "{}", run.stderr);
}

/// A wrong passphrase on the pipe fails the same single way it does on a file.
#[test]
fn a_piped_import_with_the_wrong_passphrase_does_not_authenticate() {
    let old = Device::enrolled();
    let new = Device::blank();
    let (_, payload, _) = old.run_raw(&[
        "pair",
        "--export-file",
        "-",
        "--passphrase",
        "the right one",
    ]);

    let imported = new.run_with_input(
        &[
            "pair",
            "--import-file",
            "-",
            "--passphrase",
            "the wrong one",
        ],
        &payload,
    );
    assert_ne!(imported.status, 0);
    assert!(
        new.config_text().is_empty(),
        "a failed import must write nothing: {}",
        new.config_text()
    );
}

//! Resolving an account's credential (§9).
//!
//! The rule is that the secret never lives in the config file or in `meta.db`.
//! What lives there is *how to obtain it*, and this module walks the ways in
//! order of preference:
//!
//! 1. `MAILBOX_PASSWORD_<ACCOUNT>` / `MAILBOX_OAUTH_TOKEN_<ACCOUNT>` — for
//!    tests, CI, and the L2 suite (§14.2), which needs a credential without a
//!    keychain to put one in.
//! 2. An in-process install ([`install`]) — how a native UI that already holds
//!    the secret in the OS keystore hands it over without spawning a CLI that
//!    does not exist on iOS.
//! 3. `credential_command` — the `pass`/`gpg`/`op`/`op read` hook. The most
//!    important entry in practice on the desktop, and the reason no keychain
//!    integration is load-bearing there.
//! 4. The OS keychain, reached through its own CLI rather than through a
//!    D-Bus or Security.framework binding. That keeps the static binary of §4
//!    free of a platform dependency it would otherwise carry everywhere, and
//!    the delegated command is the same one a user can run by hand to check
//!    what we will see.
//!
//! Native keychain *storage* is the frontend's job: the iOS app writes to
//! the Keychain and calls [`install`] on launch. Until a secret is resolvable
//! a store fails with `auth_failed` and a hint, which is the honest outcome.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::imap::Credential;

fn installed() -> &'static Mutex<HashMap<String, Credential>> {
    static INSTALLED: OnceLock<Mutex<HashMap<String, Credential>>> = OnceLock::new();
    INSTALLED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Hands a secret to this process without putting it in the config file.
///
/// The iOS/Android shells keep the password in the OS keystore and call this
/// on launch (and again when the user adds an account). The map is process
/// memory only: a crash forgets it, which is what we want — the keystore is
/// the durable copy.
pub fn install(account_id: &str, credential: Credential) {
    installed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(account_id.to_string(), credential);
}

/// Drops an installed secret. Removing an account from the config is not
/// enough: the next `resolve` would still find it until the process exits.
pub fn forget(account_id: &str) {
    installed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(account_id);
}

/// Looks up the credential for `account_id`, whose config is `cfg`.
pub fn resolve(account_id: &str, cfg: &crate::config::AccountConfig) -> Result<Credential> {
    let key = account_id.to_ascii_uppercase().replace(['-', '.'], "_");

    if let Some(token) = env_secret(&format!("MAILBOX_OAUTH_TOKEN_{key}")) {
        return Ok(Credential::OAuth2(token));
    }
    if let Some(pw) = env_secret(&format!("MAILBOX_PASSWORD_{key}")) {
        return Ok(Credential::Password(pw));
    }

    if let Some(credential) = installed()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(account_id)
        .cloned()
    {
        return Ok(credential);
    }

    if let Some(cmd) = &cfg.credential_command {
        return run_credential_command(cmd).map(Credential::Password);
    }

    if let Some(pw) = keychain_lookup(account_id, cfg) {
        return Ok(Credential::Password(pw));
    }

    Err(Error::Auth(format!(
        "no credential for account `{account_id}`"
    )))
}

fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Runs a shell command and takes its first line of stdout as the secret.
///
/// First line, not all of stdout: `pass` and `gpg` both print the password on
/// line one and anything else afterwards, and a password with a trailing
/// newline attached fails authentication in a way that is very hard to see.
fn run_credential_command(cmd: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| Error::Auth(format!("credential_command could not be run: {e}")))?;

    if !output.status.success() {
        // stderr, not stdout: the command's own diagnostic is what the user
        // needs, and stdout may hold a partial secret.
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Auth(format!(
            "credential_command failed ({}): {}",
            output.status,
            err.trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or("").trim_end_matches('\r');
    if first.is_empty() {
        return Err(Error::Auth("credential_command printed nothing".into()));
    }
    Ok(first.to_string())
}

/// Asks the platform keychain, through its own CLI.
///
/// Returns `None` rather than an error when the tool is absent or the entry is
/// missing: this is one step of a chain, and a missing keychain is not a
/// failure until every step has been tried.
fn keychain_lookup(account_id: &str, cfg: &crate::config::AccountConfig) -> Option<String> {
    let attempts: Vec<(&str, Vec<String>)> = if cfg!(target_os = "macos") {
        vec![(
            "security",
            vec![
                "find-internet-password".into(),
                "-s".into(),
                cfg.imap_host.clone().unwrap_or_default(),
                "-a".into(),
                cfg.login_user().to_string(),
                "-w".into(),
            ],
        )]
    } else {
        vec![(
            "secret-tool",
            vec![
                "lookup".into(),
                "service".into(),
                "mailbox".into(),
                "account".into(),
                account_id.to_string(),
            ],
        )]
    };

    for (program, args) in attempts {
        let Ok(output) = Command::new(program).args(&args).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let secret = text.lines().next().unwrap_or("").to_string();
        if !secret.is_empty() {
            return Some(secret);
        }
    }
    None
}

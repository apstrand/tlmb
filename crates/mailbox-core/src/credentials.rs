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
    let key = env_key(account_id);

    if let Some(token) = env_secret(&format!("MAILBOX_OAUTH_TOKEN_{key}")) {
        return Ok(Credential::OAuth2(token));
    }
    if let Some(pw) = env_secret(&format!("MAILBOX_PASSWORD_{key}")) {
        return Ok(Credential::Password(pw));
    }

    // Before the password chain, because an account configured for OAuth2 has
    // no password to find and the chain would end in the wrong error.
    if cfg.oauth_provider.is_some() {
        return oauth_credential(account_id, cfg);
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

/// The account id as it appears in an environment variable name.
pub fn env_key(account_id: &str) -> String {
    account_id.to_ascii_uppercase().replace(['-', '.'], "_")
}

fn env_secret(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// An OAuth2 access token, refreshed when the cached one has expired (§9).
///
/// The refresh token is a credential and comes from where credentials come
/// from; the access token is a cache. A refresh happening here rather than in
/// the connection path is deliberate — it is one network call, before the
/// mail server is contacted, and a failure is `auth_failed` with the
/// provider's own words rather than a mysterious `NO` from IMAP.
fn oauth_credential(account_id: &str, cfg: &crate::config::AccountConfig) -> Result<Credential> {
    let clock = crate::Clock::from_env();
    let cached = crate::oauth::cached(account_id);
    if let Some(tokens) = &cached {
        if tokens.is_fresh(clock.now()) {
            return Ok(Credential::OAuth2(tokens.access_token.clone()));
        }
    }

    let Some(refresh_token) = refresh_token(account_id, cached.as_ref()) else {
        return Err(Error::Auth(format!(
            "account `{account_id}` has no usable OAuth2 token; run `mailbox accounts login {account_id}`"
        )));
    };

    let endpoints = crate::oauth::endpoints_for(cfg)?;
    let client_id = cfg
        .oauth_client_id
        .clone()
        .ok_or_else(|| Error::Auth(format!("account `{account_id}` has no oauth_client_id")))?;
    let refreshed = crate::oauth::refresh(
        &endpoints.token_url,
        &client_id,
        client_secret(account_id, cfg).as_deref(),
        &refresh_token,
        &clock,
    )?;
    // Best effort: a token that cannot be cached still works for this command,
    // and the next one pays for another refresh.
    let _ = crate::oauth::cache(account_id, &refreshed);
    Ok(Credential::OAuth2(refreshed.access_token))
}

/// The durable half of an OAuth2 login, from wherever it is kept (§9).
///
/// Public because pairing sends it: §8 promises a moved device does not have
/// to redo OAuth, and the *access* token that [`resolve`] returns expires in
/// an hour.
pub fn refresh_token(account_id: &str, cached: Option<&crate::oauth::Tokens>) -> Option<String> {
    env_secret(&format!(
        "MAILBOX_OAUTH_REFRESH_TOKEN_{}",
        env_key(account_id)
    ))
    .or_else(|| keychain_get(OAUTH_SERVICE, account_id))
    .or_else(|| cached.and_then(|t| t.refresh_token.clone()))
}

/// The keychain service name refresh tokens are filed under. Distinct from the
/// password entry so an account can have both without one overwriting the
/// other.
const OAUTH_SERVICE: &str = "mailbox-oauth";

/// Remembers a refresh token for an account that has no access token yet.
///
/// The receiving half of pairing: the keychain if there is one, and otherwise
/// the account's own token file, whose access token is left empty so the first
/// command refreshes into it.
pub fn adopt_refresh_token(account_id: &str, token: &str) -> Result<bool> {
    let stored = keychain_store(OAUTH_SERVICE, account_id, token);
    if !stored {
        crate::oauth::cache(
            account_id,
            &crate::oauth::Tokens {
                access_token: String::new(),
                refresh_token: Some(token.to_string()),
                expires_at: 0,
            },
        )?;
    }
    Ok(stored)
}

/// Puts a refresh token in the OS keychain, if there is one.
///
/// Returns whether it went in. `false` is not a failure: the token is also in
/// the account's own token file, which is mode 0600 inside a directory that is
/// mode 0700 — the same protection §9 gives the mail itself. A keychain is
/// better because it survives the store being copied, so it is tried first.
pub fn store_refresh_token(account_id: &str, token: &str) -> Result<bool> {
    Ok(keychain_store(OAUTH_SERVICE, account_id, token))
}

/// Reads a secret from the platform keychain by service and account.
fn keychain_get(service: &str, account_id: &str) -> Option<String> {
    let (program, args) = if cfg!(target_os = "macos") {
        (
            "security",
            vec![
                "find-generic-password".to_string(),
                "-s".into(),
                service.into(),
                "-a".into(),
                account_id.into(),
                "-w".into(),
            ],
        )
    } else {
        (
            "secret-tool",
            vec![
                "lookup".to_string(),
                "service".into(),
                service.into(),
                "account".into(),
                account_id.into(),
            ],
        )
    };
    let output = Command::new(program).args(&args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let secret = text.lines().next().unwrap_or("").to_string();
    (!secret.is_empty()).then_some(secret)
}

/// Writes a secret to the platform keychain. `false` when there is none.
fn keychain_store(service: &str, account_id: &str, secret: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;

    if cfg!(target_os = "macos") {
        // `-U` updates an existing entry rather than failing on it. The secret
        // goes in as an argument here because `security` has no stdin form —
        // one of the reasons `secret-tool` is preferred where both exist.
        return Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                service,
                "-a",
                account_id,
                "-w",
                secret,
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }

    // stdin, so the token never appears in `ps` or in the shell history.
    let Ok(mut child) = Command::new("secret-tool")
        .args([
            "store",
            "--label",
            &format!("mailbox OAuth2 ({account_id})"),
            "service",
            service,
            "account",
            account_id,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(secret.as_bytes()).is_err() {
            return false;
        }
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// The client secret, from the environment first so it need not be in the
/// config file at all.
pub fn client_secret(account_id: &str, cfg: &crate::config::AccountConfig) -> Option<String> {
    env_secret(&format!(
        "MAILBOX_OAUTH_CLIENT_SECRET_{}",
        env_key(account_id)
    ))
    .or_else(|| cfg.oauth_client_secret.clone())
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

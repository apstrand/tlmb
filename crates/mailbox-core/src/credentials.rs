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
//! Where the platform has a keychain and the frontend has no keystore API of
//! its own — the desktop — [`store_password`] is how a password given to
//! `accounts add`, to the web interface's setup form, or to pairing becomes
//! durable. iOS does its own storage through the Keychain and calls
//! [`install`] on launch. Until a secret is resolvable a store fails with
//! `auth_failed` and a hint, which is the honest outcome.
//!
//! `MAILBOX_KEYCHAIN=none` takes the keychain out of the chain entirely, for
//! reads and writes alike. That is what keeps the suite off the desktop (§14):
//! without it, running the tests files every fixture password in the
//! developer's own keyring.

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

/// Whether the platform keychain may be consulted or written at all.
///
/// Named like [`crate::open`]'s `MAILBOX_OPENER` and §11's `MAILBOX_NOTIFIER`,
/// and for the same reason: this reaches out of the process at something the
/// user owns, so there has to be a way to say "not here".
fn keychain_enabled() -> bool {
    !matches!(std::env::var("MAILBOX_KEYCHAIN").as_deref(), Ok("none"))
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
    let client = crate::oauth::client_for(cfg, client_secret(account_id, cfg).as_deref())
        .ok_or_else(|| {
            Error::Auth(format!(
                "account `{account_id}` has no OAuth2 client id, and this build ships none"
            ))
        })?;
    let refreshed = crate::oauth::refresh(
        &endpoints.token_url,
        &client.id,
        client.secret.as_deref(),
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
    if !keychain_enabled() {
        return None;
    }
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

/// Writes a refresh token to the platform keychain. `false` when there is none.
fn keychain_store(service: &str, account_id: &str, secret: &str) -> bool {
    if !keychain_enabled() {
        return false;
    }
    if cfg!(target_os = "macos") {
        // `-U` updates an existing entry rather than failing on it. The secret
        // goes in as an argument here because `security` has no stdin form —
        // one of the reasons `secret-tool` is preferred where both exist.
        return security(&[
            "add-generic-password",
            "-U",
            "-s",
            service,
            "-a",
            account_id,
            "-w",
            secret,
        ]);
    }
    secret_tool_store(
        &format!("mailbox OAuth2 ({account_id})"),
        service,
        account_id,
        secret,
    )
}

/// The keychain service name account passwords are filed under on Linux.
///
/// It has to match what [`keychain_lookup`] reads, which is the whole reason
/// this is one constant rather than a literal at each end.
const PASSWORD_SERVICE: &str = "mailbox";

/// Files an account's password where [`resolve`] will find it again.
///
/// The durable half of [`install`]. Without it a password given to
/// `accounts add` or to the web interface's setup form lives in this
/// process's memory and nowhere else, so the account works until the program
/// exits and is broken afterwards — which is a trap for anyone who cannot
/// reach a terminal to fix it.
///
/// Returns whether it went in. `false` means no keychain (a headless box, a
/// locked keyring, `MAILBOX_KEYCHAIN=none`) and is not an error: the caller
/// says so, and §9's other routes — `credential_command`, `MAILBOX_PASSWORD_*`
/// — are still open.
pub fn store_password(account_id: &str, cfg: &crate::config::AccountConfig, secret: &str) -> bool {
    if !keychain_enabled() || secret.is_empty() {
        return false;
    }
    if cfg!(target_os = "macos") {
        // The *internet* password form, keyed by host and login, because that
        // is the form `keychain_lookup` reads — and the form the user sees in
        // Keychain Access next to every other entry for that server.
        return security(&[
            "add-internet-password",
            "-U",
            "-s",
            cfg.imap_host.as_deref().unwrap_or_default(),
            "-a",
            cfg.login_user(),
            "-w",
            secret,
        ]);
    }
    secret_tool_store(
        &format!("mailbox ({account_id})"),
        PASSWORD_SERVICE,
        account_id,
        secret,
    )
}

/// What to call the place [`store_password`] puts things, for a caller that
/// has to tell someone where their password went.
///
/// Named concretely rather than "the keychain", because "the keychain" is not
/// somewhere anyone can go and look. These are: one is a tab in Keychain
/// Access, the other is a row in Seahorse and a `secret-tool lookup` away.
pub fn keychain_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "the macOS Keychain"
    } else {
        "the login keyring (service \"mailbox\")"
    }
}

fn security(args: &[&str]) -> bool {
    Command::new("security")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `secret-tool store`, with the secret on stdin.
///
/// stdin rather than an argument, so it never appears in `ps` or in a shell
/// history — the reason `secret-tool` is preferred where both tools exist.
fn secret_tool_store(label: &str, service: &str, account_id: &str, secret: &str) -> bool {
    use std::io::Write;
    use std::process::Stdio;

    let Ok(mut child) = Command::new("secret-tool")
        .args([
            "store", "--label", label, "service", service, "account", account_id,
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

/// The configured client secret, from the environment first so it need not be
/// in the config file at all.
///
/// `None` here does not mean "no secret": the build may ship one, which
/// [`crate::oauth::client_for`] applies. Only the *configured* one is read
/// here, because only that one has an environment override.
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
    if !keychain_enabled() {
        return None;
    }
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
                PASSWORD_SERVICE.into(),
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

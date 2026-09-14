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
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use crate::error::{Error, Result};
use crate::imap::Credential;

static KEYCHAIN_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns off every keychain read and write for the rest of this process.
///
/// For an in-process caller that cannot set an environment variable safely —
/// a test binary, above all, where `set_var` races every other thread's
/// `getenv`. There is no way back on purpose: this is a safety catch, not a
/// setting.
pub fn disable_keychain() {
    KEYCHAIN_OFF.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the OS keychain may be touched at all.
///
/// `MAILBOX_KEYCHAIN=none` turns every read and write here into a miss, and
/// [`disable_keychain`] does the same from inside the process. It is the same
/// kind of switch as `MAILBOX_NOTIFIER=none` and for the same reason: `cargo
/// test` runs `accounts add --password-stdin`, and a suite that writes fixture
/// passwords into the keyring of whoever ran it — and reads back the real
/// entries of an account that happens to share an id — is a suite that has
/// escaped onto the developer's desktop.
fn keychain_enabled() -> bool {
    if KEYCHAIN_OFF.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    !matches!(
        std::env::var("MAILBOX_KEYCHAIN").as_deref(),
        Ok("none") | Ok("off")
    )
}

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

/// The keychain service name passwords are filed under, matching what
/// [`keychain_lookup`] reads back.
const PASSWORD_SERVICE: &str = "mailbox";

/// Where a password ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// The OS keychain has it, and it survives this process.
    Keychain,
    /// No keychain answered, so it is in this process's memory and no further.
    Process,
}

impl Stored {
    /// What to tell the person who just typed the password.
    pub fn describe(self) -> &'static str {
        match self {
            Stored::Keychain => "the OS keychain",
            Stored::Process => {
                "this process only — no keychain took it; set `credential_command`, \
                 or export MAILBOX_PASSWORD_<ACCOUNT>"
            }
        }
    }
}

/// Puts a password where the next process will find it (§9).
///
/// The keychain first, and this process's memory when there is none — which
/// is honest rather than useless: it is enough to finish a setup and run
/// `accounts test` against a real server, and [`Stored`] says which happened
/// so the caller does not imply a durability it did not get.
///
/// Note what is *not* here: writing the secret to the config file. §9 is
/// unambiguous, and a fallback that quietly did it would make every other
/// promise in this module worthless.
pub fn store_password(
    account_id: &str,
    cfg: &crate::config::AccountConfig,
    password: &str,
) -> Stored {
    install(account_id, Credential::Password(password.to_string()));
    if store_password_in_keychain(account_id, cfg, password) {
        Stored::Keychain
    } else {
        Stored::Process
    }
}

/// The write matching [`keychain_lookup`]'s read, per platform.
///
/// macOS files mail passwords as *internet* passwords keyed by host and login
/// name, which is where its own Mail.app looks and what makes an entry added
/// here visible in Keychain Access as what it is. Secret Service has no such
/// convention, so the service/account pair is ours.
fn store_password_in_keychain(
    account_id: &str,
    cfg: &crate::config::AccountConfig,
    password: &str,
) -> bool {
    if !keychain_enabled() {
        return false;
    }
    if cfg!(target_os = "macos") {
        let Some(host) = cfg.imap_host.clone().filter(|h| !h.is_empty()) else {
            return false;
        };
        return Command::new("security")
            .args([
                "add-internet-password",
                "-U",
                "-s",
                &host,
                "-a",
                cfg.login_user(),
                "-r",
                "imap",
                "-w",
                password,
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    }
    keychain_store(PASSWORD_SERVICE, "password", account_id, password)
}

/// Removes an account's stored password from the keychain.
fn keychain_forget_password(account_id: &str, cfg: &crate::config::AccountConfig) {
    if !keychain_enabled() {
        return;
    }
    if cfg!(target_os = "macos") {
        let Some(host) = cfg.imap_host.clone().filter(|h| !h.is_empty()) else {
            return;
        };
        let _ = Command::new("security")
            .args([
                "delete-internet-password",
                "-s",
                &host,
                "-a",
                cfg.login_user(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return;
    }
    let _ = Command::new("secret-tool")
        .args(["clear", "service", PASSWORD_SERVICE, "account", account_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Forgets everything stored for an account, keychain included.
///
/// Separate from [`forget`], which only drops the in-process copy: removing an
/// account should not leave its password in the keychain, and a caller that
/// only wants the process copy gone (a test) should not touch the keychain.
pub fn forget_stored(account_id: &str, cfg: &crate::config::AccountConfig) {
    forget(account_id);
    keychain_forget_password(account_id, cfg);
}

/// Remembers a refresh token for an account that has no access token yet.
///
/// The receiving half of pairing: the keychain if there is one, and otherwise
/// the account's own token file, whose access token is left empty so the first
/// command refreshes into it.
pub fn adopt_refresh_token(account_id: &str, token: &str) -> Result<bool> {
    let stored = keychain_store(OAUTH_SERVICE, "OAuth2", account_id, token);
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
    Ok(keychain_store(OAUTH_SERVICE, "OAuth2", account_id, token))
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

/// Writes a secret to the platform keychain. `false` when there is none.
fn keychain_store(service: &str, label: &str, account_id: &str, secret: &str) -> bool {
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
        &format!("mailbox {label} ({account_id})"),
        service,
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

/// The keychain service the §5.4 state secret is filed under.
const STATE_SERVICE: &str = "mailbox-state";

/// The name the state secret is filed under.
///
/// One secret for the whole workspace, not one per account — which is what the
/// pairing payload's single `state_key` field has always implied, and what
/// §8.3's revocation requires: revoking a device rotates *the* key, because a
/// device is revoked from the user's mail, not from one account of it.
const STATE_ENTRY: &str = "workspace";

/// The §5.4 state secret, from wherever it is kept.
///
/// Same chain as every other credential, for the same reason: the environment
/// first so a test can supply one, then the keychain, then a private file in
/// the state directory. The file exists because §5.4 is on by default and a
/// desktop with no Secret Service would otherwise silently have no
/// cross-device state at all — it is mode 0600 beside the mail it describes,
/// which is the protection §9 gives the store itself.
pub fn state_secret() -> Option<[u8; crate::state::wire::SECRET_BYTES]> {
    let hex = env_secret("MAILBOX_STATE_SECRET")
        .or_else(|| keychain_get(STATE_SERVICE, STATE_ENTRY))
        .or_else(|| std::fs::read_to_string(state_secret_path()).ok())?;
    let bytes = crate::hex::decode(hex.trim())?;
    bytes.try_into().ok()
}

/// Stores the state secret, and reports where it went.
pub fn store_state_secret(secret: &[u8]) -> Result<Stored> {
    let hex = crate::hex::encode(secret);
    if keychain_store(STATE_SERVICE, "state key", STATE_ENTRY, &hex) {
        // Belt and braces would be a second copy to delete on rotation, so the
        // file is removed rather than left holding a superseded key.
        let _ = std::fs::remove_file(state_secret_path());
        return Ok(Stored::Keychain);
    }
    let path = state_secret_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::paths::write_private(&path, hex.as_bytes())?;
    Ok(Stored::Process)
}

/// The state secret, generating and storing one if this is the first time.
///
/// A device that pairs receives the secret instead ([`adopt_state_secret`]);
/// generating one here is what the *first* device does, and doing it lazily is
/// what keeps an account that never syncs state from having a key at all.
pub fn ensure_state_secret(rng: &crate::Rng) -> Result<[u8; crate::state::wire::SECRET_BYTES]> {
    if let Some(secret) = state_secret() {
        return Ok(secret);
    }
    let secret = crate::state::wire::generate_secret(rng);
    store_state_secret(&secret)?;
    Ok(secret)
}

/// Takes the state secret from a pairing payload (§8).
///
/// The promise §8 makes is that a paired device can read the state a moment
/// later, so this overwrites whatever this device had: a device that generated
/// its own secret before pairing was talking to nobody.
pub fn adopt_state_secret(secret: &[u8]) -> Result<Stored> {
    store_state_secret(secret)
}

/// Forgets the state secret. Used by rotation (§8.3), which stores the new one
/// immediately afterwards.
pub fn forget_state_secret() {
    if keychain_enabled() {
        let _ = Command::new("secret-tool")
            .args(["clear", "service", STATE_SERVICE, "account", STATE_ENTRY])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = std::fs::remove_file(state_secret_path());
}

fn state_secret_path() -> std::path::PathBuf {
    crate::paths::state_dir().join("state.key")
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

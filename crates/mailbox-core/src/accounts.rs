//! Adding, changing, and removing accounts (§9).
//!
//! Here rather than in a frontend because there are four of them and this is
//! the code that decides what a valid account is. The rules — an id derived
//! from the address, a provider table consulted only for what the caller did
//! not supply, a refusal to write an account that cannot possibly connect —
//! are the kind that drift apart the moment they are written twice.
//!
//! What this module will not do is put a secret in the config. §9 is
//! unambiguous: the config says *how* to obtain a credential, and the
//! credential itself goes to the keychain, a command, or the process's own
//! memory ([`crate::credentials`]). `add` therefore takes a password and hands
//! it straight on rather than storing it, and the result says where it went.

use std::path::Path;

use crate::config::{AccountConfig, Config};
use crate::error::{Error, Result};

/// What a caller knows about the account it wants to add.
///
/// Everything but the address is optional: the provider table fills in what it
/// can, and what neither the caller nor the table supplies is an error rather
/// than a guess.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NewAccount {
    /// The config key. Derived from the address when empty.
    #[serde(default)]
    pub id: String,
    pub address: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<String>,
    pub imap_user: Option<String>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<String>,
    pub smtp_user: Option<String>,
    /// Handed to [`crate::credentials::install`], never written to the config.
    pub password: Option<String>,
    /// §9's `pass`/`gpg`/`op` hook, which *is* config: it is a command, not a
    /// secret.
    pub credential_command: Option<String>,
    pub require_tls: Option<bool>,
    /// Which OAuth2 provider to authenticate with, if any (§9).
    pub oauth_provider: Option<String>,
    /// The client id registered for this installation. Public by design for a
    /// native app — PKCE is what protects the flow — so it belongs in config.
    pub oauth_client_id: Option<String>,
}

/// What `add` did, for the caller to report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Added {
    pub id: String,
    pub address: String,
    /// True when the provider table supplied hosts the caller did not.
    pub discovered: bool,
    /// Where the credential will come from, or `null` when nothing was given
    /// and the account cannot yet connect.
    pub credential: Option<String>,
}

/// Adds an account to the config at `path`.
pub fn add(path: &Path, spec: &NewAccount) -> Result<Added> {
    let address = spec.address.trim().to_string();
    if address.is_empty() {
        return Err(Error::usage("an account needs an address"));
    }
    if !address.contains('@') {
        return Err(Error::usage(format!("`{address}` is not an email address")));
    }
    let id = match spec.id.trim() {
        "" => id_from_address(&address),
        given => normalize_id(given)?,
    };

    // Loaded before anything else fails, so a broken config is reported as
    // itself rather than as a failure to add.
    let mut config = load_or_empty(path)?;
    if config.accounts.contains_key(&id) {
        return Err(Error::usage(format!(
            "account `{id}` already exists; pass an id to add a second one for the same address"
        )));
    }

    let hints = crate::providers::hints_for(&address);
    let discovered = hints.is_some() && spec.imap_host.is_none();

    let mut cfg = AccountConfig {
        address: address.clone(),
        imap_host: spec
            .imap_host
            .clone()
            .or_else(|| hints.as_ref().map(|h| h.imap_host.clone())),
        imap_port: spec
            .imap_port
            .or_else(|| hints.as_ref().map(|h| h.imap_port)),
        imap_security: spec
            .imap_security
            .clone()
            .or_else(|| hints.as_ref().map(|h| h.imap_security.to_string())),
        imap_user: spec.imap_user.clone(),
        smtp_host: spec
            .smtp_host
            .clone()
            .or_else(|| hints.as_ref().map(|h| h.smtp_host.clone())),
        smtp_port: spec
            .smtp_port
            .or_else(|| hints.as_ref().map(|h| h.smtp_port)),
        smtp_security: spec
            .smtp_security
            .clone()
            .or_else(|| hints.as_ref().map(|h| h.smtp_security.to_string())),
        smtp_user: spec.smtp_user.clone(),
        credential_command: spec.credential_command.clone(),
        oauth_provider: spec.oauth_provider.clone(),
        oauth_client_id: spec.oauth_client_id.clone(),
        ..Default::default()
    };
    if spec.require_tls == Some(false) {
        cfg.require_tls = false;
    }

    // An account with no IMAP host is not an account, and writing one would
    // trade an error here for a confusing one on the next command.
    if cfg.imap_host.as_deref().unwrap_or("").is_empty() {
        return Err(Error::usage(format!(
            "no built-in settings for `{}` — give at least --imap-host",
            address.rsplit('@').next().unwrap_or(&address)
        )));
    }
    validate(&cfg)?;

    config.accounts.insert(id.clone(), cfg);
    config.save(path)?;

    // Only after the config is written: a credential installed for an account
    // that failed to save would be a secret held for nothing.
    let credential = if let Some(password) = spec.password.clone().filter(|p| !p.is_empty()) {
        crate::credentials::install(&id, crate::imap::Credential::Password(password));
        Some("installed for this process".to_string())
    } else if spec.credential_command.is_some() {
        Some("credential_command".to_string())
    } else if spec.oauth_provider.is_some() {
        Some("oauth2 (run `mailbox accounts login`)".to_string())
    } else {
        None
    };

    Ok(Added {
        id,
        address,
        discovered,
        credential,
    })
}

/// Changes fields on an existing account. `None` leaves a field alone.
///
/// Deliberately not "replace the account": every frontend that edits one edits
/// a field or two, and a whole-config write from a form that did not know
/// about `identities` would silently drop them.
pub fn edit(path: &Path, id: &str, spec: &NewAccount) -> Result<AccountConfig> {
    let mut config = Config::load(path)?;
    let cfg = config
        .accounts
        .get_mut(id)
        .ok_or_else(|| Error::not_found("account", id))?;

    if !spec.address.trim().is_empty() {
        cfg.address = spec.address.trim().to_string();
    }
    set(&mut cfg.imap_host, &spec.imap_host);
    set(&mut cfg.imap_user, &spec.imap_user);
    set(&mut cfg.imap_security, &spec.imap_security);
    set(&mut cfg.smtp_host, &spec.smtp_host);
    set(&mut cfg.smtp_user, &spec.smtp_user);
    set(&mut cfg.smtp_security, &spec.smtp_security);
    set(&mut cfg.credential_command, &spec.credential_command);
    set(&mut cfg.oauth_provider, &spec.oauth_provider);
    set(&mut cfg.oauth_client_id, &spec.oauth_client_id);
    if let Some(port) = spec.imap_port {
        cfg.imap_port = Some(port);
    }
    if let Some(port) = spec.smtp_port {
        cfg.smtp_port = Some(port);
    }
    if let Some(require) = spec.require_tls {
        cfg.require_tls = require;
    }

    let updated = cfg.clone();
    validate(&updated)?;
    config.save(path)?;

    if let Some(password) = spec.password.clone().filter(|p| !p.is_empty()) {
        crate::credentials::install(id, crate::imap::Credential::Password(password));
    }
    Ok(updated)
}

/// Removes an account from the config, and forgets its credential.
///
/// The *store* is left alone. Deleting a mailbox's local cache because someone
/// mistyped an id is not recoverable from the config file, and the directory
/// is named in the result so it can be removed deliberately.
pub fn remove(path: &Path, id: &str) -> Result<std::path::PathBuf> {
    let mut config = Config::load(path)?;
    if config.accounts.remove(id).is_none() {
        return Err(Error::not_found("account", id));
    }
    config.save(path)?;
    crate::credentials::forget(id);
    crate::oauth::forget_tokens(id);
    Ok(crate::paths::account_dir(id))
}

/// Rejects a configuration that cannot work, before it is written.
fn validate(cfg: &AccountConfig) -> Result<()> {
    for (name, value) in [
        ("imap_security", &cfg.imap_security),
        ("smtp_security", &cfg.smtp_security),
    ] {
        if let Some(v) = value {
            if !matches!(v.as_str(), "implicit" | "starttls" | "none" | "tls" | "ssl") {
                return Err(Error::usage(format!(
                    "`{v}` is not a {name}; use implicit, starttls, or none"
                )));
            }
        }
    }

    // §9: plaintext is refused unless the host is loopback. Catching it here
    // means the refusal names the setting rather than arriving as a connection
    // error on the first sync.
    let plaintext = cfg.imap_security.as_deref() == Some("none");
    if (plaintext || !cfg.require_tls) && !is_loopback(cfg.imap_host.as_deref().unwrap_or("")) {
        return Err(Error::usage(format!(
            "`{}` is not a loopback host, so TLS cannot be turned off for it (§9)",
            cfg.imap_host.as_deref().unwrap_or("")
        )));
    }
    if cfg.oauth_provider.is_some() && cfg.oauth_client_id.is_none() {
        return Err(Error::usage(
            "an OAuth2 account needs oauth_client_id — see `mailbox accounts login --help`",
        ));
    }
    if let Some(provider) = &cfg.oauth_provider {
        crate::oauth::provider(provider)?;
    }
    Ok(())
}

pub fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        || host.starts_with("127.")
        || host.ends_with(".localhost")
}

fn set(field: &mut Option<String>, value: &Option<String>) {
    if let Some(v) = value {
        *field = Some(v.clone());
    }
}

fn load_or_empty(path: &Path) -> Result<Config> {
    if path.exists() {
        Config::load(path)
    } else {
        Ok(Config::default())
    }
}

/// The config key for an address, when the caller did not choose one.
///
/// The local part, lowercased, with anything that is not a plain identifier
/// character dropped — this ends up in `--account`, in an environment variable
/// name, and in a directory name, and the intersection of what all three
/// accept is narrow.
pub fn id_from_address(address: &str) -> String {
    let local = address.split('@').next().unwrap_or(address);
    let s: String = local
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect::<String>()
        .to_ascii_lowercase();
    if s.is_empty() {
        "account".into()
    } else {
        s
    }
}

fn normalize_id(given: &str) -> Result<String> {
    let id = given.to_ascii_lowercase();
    if id
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    {
        return Err(Error::usage(format!(
            "`{given}` is not a usable account id; letters, digits, `-`, `_`, and `.` only"
        )));
    }
    Ok(id)
}

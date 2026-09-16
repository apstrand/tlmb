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
    /// Look the servers up on the network ([`crate::discover`]) when the
    /// caller did not supply them.
    ///
    /// Off by default, and opt-in rather than opt-out, because this struct is
    /// also how a test and a paired device add an account: neither wants a
    /// setup that reaches the network without having been asked to. The CLI
    /// and the setup screens turn it on, which is where a person did ask.
    #[serde(default)]
    pub discover: bool,
}

/// What `add` did, for the caller to report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Added {
    pub id: String,
    pub address: String,
    /// True when discovery supplied hosts the caller did not.
    pub discovered: bool,
    /// Which of §9's sources those hosts came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::discover::Source>,
    /// Whether an IMAP connection to the discovered host actually succeeded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verified: bool,
    /// Where the credential will come from, or `null` when nothing was given
    /// and the account cannot yet connect.
    pub credential: Option<String>,
}

struct Prepared {
    id: String,
    config: AccountConfig,
    discovered: bool,
    source: Option<crate::discover::Source>,
    verified: bool,
}

/// The id and config `add` would write, without writing anything.
///
/// Split out because the setup form in the web interface has to be able to
/// *try* an account before committing to it (see [`verify`]): an account
/// written with a password that does not work is one that whoever filled the
/// form in cannot remove again without a terminal — which is exactly the
/// person who has no terminal.
pub fn prepare(spec: &NewAccount) -> Result<(String, AccountConfig, bool)> {
    let prepared = prepare_details(spec)?;
    Ok((prepared.id, prepared.config, prepared.discovered))
}

fn prepare_details(spec: &NewAccount) -> Result<Prepared> {
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

    // Discovery runs only for what the caller left open: someone who typed
    // `--imap-host` has answered the question already.
    let discovery = if spec.discover && spec.imap_host.is_none() {
        Some(crate::discover::discover(
            &address,
            &crate::discover::Options::from_env(),
        )?)
    } else {
        None
    };
    let found = discovery.as_ref().and_then(|d| d.found.clone());
    let hints = match &found {
        Some(found) => Some(crate::providers::ServerHints::from(found.hints.clone())),
        None => crate::providers::hints_for(&address),
    };
    let discovered = hints.is_some() && spec.imap_host.is_none();
    let source = found.as_ref().map(|f| f.source).or(if discovered {
        Some(crate::discover::Source::BuiltIn)
    } else {
        None
    });
    let verified = found.as_ref().is_some_and(|f| f.verified);

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
        imap_user: given(&spec.imap_user)
            .or_else(|| hints.as_ref().and_then(|h| h.imap_user.clone())),
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
        smtp_user: given(&spec.smtp_user)
            .or_else(|| hints.as_ref().and_then(|h| h.smtp_user.clone())),
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
        return Err(no_servers(&address, discovery.as_ref()));
    }
    validate(&cfg)?;
    Ok(Prepared {
        id,
        config: cfg,
        discovered,
        source,
        verified,
    })
}

/// What a successful [`verify`] found, for the caller to report.
///
/// The same three things `accounts test` prints, because they are the three
/// that answer the question someone asks after "did it work": what the server
/// offers, how much mail is behind it, and — since §5.3's fast paths are the
/// difference between a sync that takes a second and one that takes a minute
/// — which path this account will sync by.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verified {
    pub server: String,
    pub login_user: String,
    pub folders: usize,
    pub capabilities: Vec<String>,
    pub sync_path: String,
}

/// Connects to an account's IMAP server, authenticates, and looks around.
///
/// The check `accounts test` makes, available *before* the account exists —
/// which is what lets the web interface's setup form offer a Test button, and
/// what lets its Add refuse to write an account that could not have worked.
///
/// The connection is closed again immediately. Nothing is stored, and the
/// account need not be in any config.
pub fn verify(
    id: &str,
    cfg: &AccountConfig,
    credential: &crate::imap::Credential,
    tracer: crate::Tracer,
    timeout: std::time::Duration,
) -> Result<Verified> {
    let host = cfg
        .imap_host
        .clone()
        .ok_or_else(|| Error::usage(format!("account `{id}` has no imap_host")))?;

    let conn = crate::imap::ConnectionConfig {
        host: host.clone(),
        port: cfg.port(),
        security: cfg.security(),
        // The same both-halves rule `Session::connect` applies: a config that
        // turns TLS off for a public host still gets TLS (§9).
        require_tls: cfg.require_tls || !is_loopback(&host),
        timeout,
    };

    let mut client = crate::imap::Client::connect(&conn, tracer)?;
    client.login(cfg.login_user(), credential)?;

    let folders = client.list_folders()?.len();
    let capabilities: Vec<String> = client.capabilities().iter().cloned().collect();
    let qresync = client.qresync();
    let _ = client.logout();

    Ok(Verified {
        server: format!("{host}:{}", cfg.port()),
        login_user: cfg.login_user().to_string(),
        folders,
        sync_path: if qresync {
            "qresync".to_string()
        } else if capabilities.iter().any(|c| c == "CONDSTORE") {
            "condstore".to_string()
        } else {
            "full-listing".to_string()
        },
        capabilities,
    })
}

/// Adds an account to the config at `path`.
pub fn add(path: &Path, spec: &NewAccount) -> Result<Added> {
    let Prepared {
        mut id,
        config: cfg,
        discovered,
        source,
        verified,
    } = prepare_details(spec)?;

    // Loaded before anything else fails, so a broken config is reported as
    // itself rather than as a failure to add.
    let mut config = load_or_empty(path)?;
    if config.accounts.contains_key(&id) && spec.id.trim().is_empty() {
        let email_id = normalize_id(&cfg.address)?;
        if !config.accounts.contains_key(&email_id) {
            id = email_id;
        }
    }
    if config.accounts.contains_key(&id) {
        return Err(Error::usage(format!(
            "account `{id}` already exists; pass a different id to add another account"
        )));
    }
    let address = cfg.address.clone();

    config.accounts.insert(id.clone(), cfg);
    config.save(path)?;

    // Only after the config is written: a credential installed for an account
    // that failed to save would be a secret held for nothing.
    let credential = if let Some(password) = spec.password.clone().filter(|p| !p.is_empty()) {
        Some(remember_password(&id, config.account(&id)?, &password))
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
        source,
        verified,
        credential,
    })
}

/// Holds a password for this process, and durably where the platform allows.
///
/// Both halves, always. The keychain is what makes the account still work
/// tomorrow; the in-process copy is what makes it work *now*, before anything
/// has re-read the keychain, and on the machines where there is no keychain to
/// read. The returned string is what the caller tells the user, and the
/// difference matters to them: one of these outcomes survives a restart.
fn remember_password(id: &str, cfg: &AccountConfig, password: &str) -> String {
    let store = crate::credentials::keychain_name();
    match crate::credentials::store_password(id, cfg, password) {
        crate::credentials::Stored::Keychain => format!("stored in {store}"),
        crate::credentials::Stored::Process => {
            format!("installed for this session only — this process only; {store} did not take it")
        }
    }
}

/// The failure §9's one-line setup has to explain well: nothing was found.
fn no_servers(address: &str, discovery: Option<&crate::discover::Discovery>) -> Error {
    let domain = address.rsplit('@').next().unwrap_or(address);
    let mut message =
        format!("no server settings found for `{domain}` — give at least --imap-host");
    if let Some(discovery) = discovery {
        for step in &discovery.steps {
            message.push_str(&format!("\n  {}: {}", step.target, step.outcome));
        }
    }
    Error::usage(message)
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
        remember_password(id, &updated, &password);
    }
    Ok(updated)
}

/// Selects which folder wins when the server assigns one special-use role to
/// multiple folders. This is a client preference, so it belongs in the account
/// config and survives a refresh of the server's folder metadata.
pub fn prefer_folder(path: &Path, id: &str, use_name: &str, folder: Option<&str>) -> Result<()> {
    let mut config = Config::load(path)?;
    let cfg = config
        .accounts
        .get_mut(id)
        .ok_or_else(|| Error::not_found("account", id))?;
    if let Some(folder) = folder {
        cfg.folder_use
            .insert(use_name.to_string(), folder.to_string());
    } else {
        cfg.folder_use.remove(use_name);
    }
    config.save(path)
}

/// Removes an account from the config, and forgets its credential.
///
/// The *store* is left alone. Deleting a mailbox's local cache because someone
/// mistyped an id is not recoverable from the config file, and the directory
/// is named in the result so it can be removed deliberately.
pub fn remove(path: &Path, id: &str) -> Result<std::path::PathBuf> {
    let mut config = Config::load(path)?;
    let Some(cfg) = config.accounts.remove(id) else {
        return Err(Error::not_found("account", id));
    };
    config.save(path)?;
    // The keychain too: an account removed from the config that left its
    // password behind is a secret nothing will ever ask for again.
    crate::credentials::forget_stored(id, &cfg);
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
    if let Some(id) = &cfg.oauth_provider {
        let provider = crate::oauth::provider(id)?;
        // A client id is only required when the build ships none: a release
        // that carries its own registration should let `--oauth google` be the
        // whole of it (§16).
        if cfg.oauth_client_id.is_none() && provider.builtin.is_none() {
            return Err(Error::usage(format!(
                "this build ships no {} client, so an OAuth2 account needs oauth_client_id. \
                 Register an application at {} and pass --oauth-client-id",
                provider.name, provider.register_url
            )));
        }
    }
    Ok(())
}

pub fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        || host.starts_with("127.")
        || host.ends_with(".localhost")
}

/// Applies one edited field.
///
/// `None` leaves it alone — that is what "edit a field or two" means, and a
/// form that did not know about a setting must not erase it. `Some("")` is the
/// other thing a caller needs and had no way to say: put it back to the
/// default. Without it a login name typed by mistake, or filled in by
/// discovery from an autoconfig document, could be changed but never removed.
fn set(field: &mut Option<String>, value: &Option<String>) {
    match value {
        None => {}
        Some(v) if v.trim().is_empty() => *field = None,
        Some(v) => *field = Some(v.trim().to_string()),
    }
}

/// What the caller actually supplied. An empty string is not a value.
fn given(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
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
/// character dropped. If this short name conflicts, [`add`] falls back to the
/// complete email address.
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
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+')))
    {
        return Err(Error::usage(format!(
            "`{given}` is not a usable account id; letters, digits, `-`, `_`, `.`, `+`, and `@` only"
        )));
    }
    Ok(id)
}

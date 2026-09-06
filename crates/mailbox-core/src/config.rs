//! `config.toml` (§12). Everything optional, few knobs, all of them documented.
//!
//! Two properties are load-bearing:
//!
//! - **No credentials.** §9 is categorical: they never land in the config file
//!   or in `meta.db`. What can appear here is *how to obtain* one — a command
//!   to run, a keychain entry to look up — never the secret itself.
//! - **Every default is compiled in.** A missing file is a valid
//!   configuration, and `mailbox config explain` can therefore always say where
//!   a value came from.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    /// Keyed by account id — the short name `--account` takes.
    #[serde(default)]
    pub accounts: BTreeMap<String, AccountConfig>,
    #[serde(default)]
    pub threading: Threading,
    #[serde(default)]
    pub notify: Notify,
    #[serde(default)]
    pub pair: Pair,
    /// Keymap overrides. Passed through to the TUI (M3) without interpretation
    /// here, so an unknown key is that frontend's problem to report.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(default = "default_editor")]
    pub editor: String,
    #[serde(default = "default_true")]
    pub unified_inbox: bool,
    /// `immediate` | `approve` (§6.5). Defaults to `approve` under
    /// `MAILBOX_AGENT=1`, which [`Config::send_policy`] resolves.
    #[serde(default = "default_send_policy")]
    pub send_policy: String,
    /// §5.4. Effective from M8; recorded now so a user who sets it early is
    /// honoured rather than surprised later.
    #[serde(default = "default_true")]
    pub state_sync: bool,
    /// Default page size for `ls` and `search`.
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl Default for General {
    fn default() -> Self {
        General {
            editor: default_editor(),
            unified_inbox: true,
            send_policy: default_send_policy(),
            state_sync: true,
            limit: default_limit(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Threading {
    /// §5.6: JWZ's subject-based grouping of otherwise unlinked messages.
    #[serde(default = "default_true")]
    pub subject: bool,
    /// How far apart two same-subject messages may be and still be grouped.
    #[serde(default = "default_subject_days")]
    pub subject_window_days: i64,
}

impl Default for Threading {
    fn default() -> Self {
        Threading {
            subject: true,
            subject_window_days: default_subject_days(),
        }
    }
}

impl Threading {
    pub fn options(&self) -> crate::threading::ThreadingOptions {
        crate::threading::ThreadingOptions {
            subject_grouping: self.subject,
            subject_window: self.subject_window_days * 86_400,
        }
    }
}

/// Desktop notifications (§11).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Which mail is worth interrupting someone for. §11's default.
    #[serde(default = "default_notify_query")]
    pub query: String,
    /// The command to run. Discovered from the platform when unset.
    ///
    /// A command rather than a D-Bus or Cocoa binding, for the same reason as
    /// the keychain (§9): it keeps a platform dependency out of the static
    /// binary of §4, and it is the same thing a user can run by hand to see
    /// what we will do.
    #[serde(default)]
    pub command: Option<String>,
    /// Most notifications one sync pass will raise.
    ///
    /// A mailbox that has been offline for a week should not produce two
    /// hundred popups; past a handful the right message is "43 new messages".
    #[serde(default = "default_notify_max")]
    pub max_per_sync: usize,
}

impl Default for Notify {
    fn default() -> Self {
        Notify {
            enabled: true,
            query: default_notify_query(),
            command: None,
            max_per_sync: default_notify_max(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    /// §8.2. Unused until M8; parsed now so a config written for a later
    /// version does not fail to load on this one.
    #[serde(default)]
    pub relay: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub address: String,
    /// Every address this account answers to, for `to:me` and for compose's
    /// From selector. The primary address is implied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<String>,

    /// Discovered by autoconfig when absent (§9). Required for M1, which has no
    /// discovery yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_port: Option<u16>,
    /// `implicit` (993) | `starttls` (143) | `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_security: Option<String>,
    /// The login name, when it differs from `address`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap_user: Option<String>,

    /// SMTP submission (§5.5). Discovered by autoconfig when absent, which
    /// lands with `accounts add`; until then, set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_port: Option<u16>,
    /// `starttls` (587) | `implicit` (465) | `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_security: Option<String>,
    /// The submission login, when it differs from `imap_user`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp_user: Option<String>,

    /// §9: a command that prints the password on stdout. This is the
    /// `pass`/`gpg`/`op` hook, and the reason no secret needs to live here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_command: Option<String>,

    /// `google` | `microsoft` | `yahoo` — authenticate with OAuth2 rather than
    /// a password (§9). `mailbox accounts login <id>` performs the flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_provider: Option<String>,

    /// The OAuth2 client id registered for this installation.
    ///
    /// Config rather than a credential, and deliberately: for an installed
    /// application the client id is public by design — PKCE is what protects
    /// the flow — so it is an identifier, not a secret. No id ships with
    /// mailbox; see §16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_id: Option<String>,

    /// A client *secret*, for the providers that still insist on one for a
    /// native app. It is not a secret in any real sense once it is on a
    /// laptop, which is why it may also be supplied through
    /// `MAILBOX_OAUTH_CLIENT_SECRET_<ACCOUNT>` and kept out of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_client_secret: Option<String>,

    /// Overrides for a provider the built-in table does not know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_auth_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_token_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_scope: Option<String>,

    /// §9. Turning this off is only honoured for a loopback host.
    #[serde(default = "default_true")]
    pub require_tls: bool,

    /// `none` | `recent` | `full` (§5.1).
    #[serde(default = "default_offline")]
    pub offline: String,
}

/// Written out rather than derived, so that `AccountConfig::default()` and a
/// deserialized empty stanza agree.
///
/// A derived `Default` gives `require_tls: false`, while `#[serde(default =
/// "default_true")]` gives `true` — and the two are used interchangeably by
/// anything that builds a config with `..Default::default()`. That
/// disagreement is a silent downgrade to plaintext, which is the worst
/// possible field to have it on.
impl Default for AccountConfig {
    fn default() -> AccountConfig {
        AccountConfig {
            address: String::new(),
            identities: Vec::new(),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            imap_user: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            smtp_user: None,
            credential_command: None,
            oauth_provider: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_auth_url: None,
            oauth_token_url: None,
            oauth_scope: None,
            require_tls: default_true(),
            offline: default_offline(),
        }
    }
}

impl AccountConfig {
    pub fn login_user(&self) -> &str {
        self.imap_user.as_deref().unwrap_or(&self.address)
    }

    /// Every address the account answers to, primary first, deduplicated.
    pub fn all_identities(&self) -> Vec<String> {
        let mut out = vec![self.address.to_ascii_lowercase()];
        for i in &self.identities {
            let lower = i.to_ascii_lowercase();
            if !out.contains(&lower) {
                out.push(lower);
            }
        }
        out
    }

    /// The submission login. Falls back through `imap_user` to the address,
    /// because a provider that separates them usually uses the same name for
    /// both halves.
    pub fn smtp_login_user(&self) -> &str {
        self.smtp_user
            .as_deref()
            .or(self.imap_user.as_deref())
            .unwrap_or(&self.address)
    }

    pub fn smtp_port(&self) -> u16 {
        self.smtp_port.unwrap_or(match self.smtp_security() {
            crate::imap::Security::Implicit => 465,
            _ => 587,
        })
    }

    pub fn smtp_security(&self) -> crate::imap::Security {
        match self.smtp_security.as_deref() {
            Some("implicit") | Some("tls") | Some("ssl") => crate::imap::Security::Implicit,
            Some("none") | Some("plain") => crate::imap::Security::None,
            Some("starttls") => crate::imap::Security::StartTls,
            // 465 is the implicit-TLS submission port; 587 and 25 are STARTTLS.
            None => match self.smtp_port {
                Some(465) => crate::imap::Security::Implicit,
                _ => crate::imap::Security::StartTls,
            },
            Some(_) => crate::imap::Security::StartTls,
        }
    }

    pub fn port(&self) -> u16 {
        self.imap_port.unwrap_or(match self.security() {
            crate::imap::Security::Implicit => 993,
            _ => 143,
        })
    }

    pub fn security(&self) -> crate::imap::Security {
        match self.imap_security.as_deref() {
            Some("starttls") => crate::imap::Security::StartTls,
            Some("none") | Some("plain") => crate::imap::Security::None,
            Some("implicit") | Some("tls") | Some("ssl") => crate::imap::Security::Implicit,
            // Infer from the port when unset: 143 is the STARTTLS port and 993
            // the implicit one, and guessing wrong produces a confusing TLS
            // handshake failure rather than a useful message.
            None => match self.imap_port {
                Some(143) => crate::imap::Security::StartTls,
                _ => crate::imap::Security::Implicit,
            },
            Some(_) => crate::imap::Security::Implicit,
        }
    }

    /// The prefetch window in days, from `offline` (§5.1).
    pub fn prefetch_days(&self) -> Option<i64> {
        match self.offline.as_str() {
            "none" => None,
            // `full` is not "a very large window": expressing it that way
            // produced a cutoff near i64::MAX which underflowed on subtraction
            // and, wrapping, matched nothing — so the setting that means
            // "download everything" downloaded nothing. It is a separate flag.
            "full" => None,
            _ => Some(30),
        }
    }

    /// §5.1's `offline = "full"`: fetch every body, for offline use.
    pub fn wants_everything_offline(&self) -> bool {
        self.offline == "full"
    }
}

impl Config {
    /// Loads the config, or returns defaults when the file does not exist.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| {
                // A config error must say where: a user staring at a rejected
                // file needs the line, not the fact.
                Error::usage(format!("{}: {e}", path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load_default() -> Result<Config> {
        Config::load(&crate::paths::config_file())
    }

    /// Writes the config as TOML, creating parent directories.
    ///
    /// Secrets still do not belong here: [`crate::credentials::install`] is
    /// how a frontend hands over a password.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::usage(format!("{}: could not serialise: {e}", path.display())))?;
        // Owner-only: the config carries no password (§9), but it does carry
        // `oauth_client_secret`, and a mail account's server and login name are
        // not something to leave world-readable either.
        crate::paths::write_private(path, text.as_bytes())?;
        Ok(())
    }

    pub fn account(&self, id: &str) -> Result<&AccountConfig> {
        self.accounts
            .get(id)
            .ok_or_else(|| Error::not_found("account", id))
    }

    /// The account a command acts on when `--account` was not given.
    ///
    /// With exactly one configured account, that one. With several, `default`
    /// if it exists, else the alphabetically first — deterministic, so a script
    /// that omits `--account` behaves the same on every run.
    pub fn default_account(&self) -> Result<&str> {
        if self.accounts.is_empty() {
            return Err(Error::not_found("account", "any"));
        }
        if self.accounts.len() == 1 {
            return Ok(self.accounts.keys().next().unwrap());
        }
        if self.accounts.contains_key("default") {
            return Ok("default");
        }
        Ok(self.accounts.keys().next().unwrap())
    }

    /// §6.5: `approve` is the default under `MAILBOX_AGENT=1` even when the
    /// config says `immediate`, because an agent that can send unsupervised is
    /// a liability the config file should have to opt *into*, not out of.
    pub fn send_policy(&self, agent_mode: bool) -> &str {
        if agent_mode && self.general.send_policy == "immediate" {
            "approve"
        } else {
            &self.general.send_policy
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_editor() -> String {
    "$EDITOR".to_string()
}
fn default_send_policy() -> String {
    "immediate".to_string()
}
fn default_limit() -> usize {
    50
}
fn default_subject_days() -> i64 {
    14
}
fn default_notify_query() -> String {
    "is:unread in:inbox NOT tag:muted".to_string()
}
fn default_notify_max() -> usize {
    5
}
fn default_offline() -> String {
    "recent".to_string()
}

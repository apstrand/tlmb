//! Device pairing and account migration (§8).
//!
//! > `mailbox pair` transfers a **payload** — account definitions, credentials
//! > and OAuth refresh tokens, identities/aliases, the §5.4 state key, and UI
//! > preferences — from an enrolled device to a new one. It never transfers the
//! > mail store; the new device syncs that from IMAP.
//!
//! M4 ships the two offline transports and the export file. The relay and the
//! short code wait for M8 and an outside review of the PAKE, because "a 45-bit
//! code guarded by an unreviewed handshake is a credential leak, not a
//! convenience" (§16).
//!
//! The thing to keep in view when reading this module is what is in the
//! payload: every credential the user has. It is sealed before it touches a
//! socket or a file, it is never logged, and the struct deliberately has no
//! `Debug` that would print it.

pub mod crypto;
pub mod file;
pub mod pake;
pub mod relay;
pub mod transport;

use serde::{Deserialize, Serialize};

use crate::config::{AccountConfig, Config, General};
use crate::error::{Error, Result};

/// The payload's schema version, authenticated as AAD (§8.1).
///
/// A receiver that does not understand a version must refuse rather than guess:
/// a half-understood payload would install accounts with pieces missing.
pub const PAYLOAD_VERSION: u32 = 1;

/// Everything that moves between devices.
///
/// No `Debug`: this holds every credential the user has, and a stray `{:?}` in
/// a log or a panic message is exactly how they escape.
#[derive(Clone, Serialize, Deserialize)]
pub struct Payload {
    pub version: u32,
    /// Unix seconds. §8.1's freshness check compares against this.
    pub created: i64,
    /// Random per payload, so two payloads are never byte-identical and a
    /// replay is detectable even inside the TTL.
    pub nonce: String,
    /// What the sending device calls itself, shown in the confirmation.
    pub device: String,
    pub accounts: Vec<PairedAccount>,
    /// §12's general preferences, which are worth carrying and cost nothing.
    #[serde(default)]
    pub preferences: Option<General>,
    /// The §5.4 state key, hex.
    ///
    /// Absent only when the sending device has never had one — a device that
    /// has synced state has a secret, and §8 promises the receiver can read
    /// that state a moment later, which is exactly what this field buys.
    #[serde(default)]
    pub state_key: Option<String>,
}

/// One account, with the secret needed to use it.
#[derive(Clone, Serialize, Deserialize)]
pub struct PairedAccount {
    pub id: String,
    pub config: AccountConfig,
    /// The resolved credential.
    ///
    /// §9 keeps secrets out of the config file, so `credential_command` alone
    /// would not be portable — the new device may have no `pass` store. The
    /// whole point of pairing is "no re-entering passwords, no re-doing OAuth"
    /// (§1.6), so the secret itself travels.
    #[serde(default)]
    pub credential: Option<PairedCredential>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PairedCredential {
    Password {
        secret: String,
    },
    /// An OAuth2 refresh token. §1.6 is explicit that pairing must not mean
    /// redoing OAuth.
    OAuth2 {
        refresh_token: String,
    },
}

/// How an incoming payload should be applied (§8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyMode {
    /// The default: add accounts, leave existing ones alone.
    Merge,
    /// Wipe the target's accounts first. Requires `--yes` (§8.3).
    Replace,
}

/// What applying a payload did.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    /// Accounts whose credential could not be stored, so the user knows they
    /// will still be asked for it.
    pub credentials_not_stored: Vec<String>,
}

/// The credential to send for one account.
///
/// For OAuth2 this is deliberately *not* what [`crate::credentials::resolve`]
/// returns: that is an access token good for about an hour, and §8 promises
/// the new device does not have to redo the OAuth flow. What travels is the
/// refresh token, which is the thing that survives the trip.
fn paired_credential(id: &str, account: &crate::config::AccountConfig) -> Option<PairedCredential> {
    if account.oauth_provider.is_some() {
        let cached = crate::oauth::cached(id);
        return crate::credentials::refresh_token(id, cached.as_ref())
            .map(|refresh_token| PairedCredential::OAuth2 { refresh_token });
    }
    match crate::credentials::resolve(id, account).ok()? {
        crate::imap::Credential::Password(secret) => Some(PairedCredential::Password { secret }),
        // An account that resolves to a bearer token without being configured
        // for OAuth2 got it from the environment, and sending an hour of
        // access to another machine is not what anyone meant by pairing.
        crate::imap::Credential::OAuth2(_) => None,
    }
}

impl Payload {
    /// Builds the payload for the accounts in `config`.
    ///
    /// Credentials are resolved here, on the sending device, because that is
    /// the only machine where `credential_command` or the keychain will answer.
    /// An account whose credential cannot be resolved still travels — its
    /// definition is worth having — and the receiver is told which ones will
    /// need a password.
    pub fn build(config: &Config, device: &str, clock: &crate::Clock, rng: &crate::Rng) -> Payload {
        let accounts = config
            .accounts
            .iter()
            .map(|(id, account)| PairedAccount {
                id: id.clone(),
                config: account.clone(),
                credential: paired_credential(id, account),
            })
            .collect();

        Payload {
            version: PAYLOAD_VERSION,
            created: clock.now(),
            nonce: rng.hex(16),
            device: device.to_string(),
            accounts,
            preferences: Some(config.general.clone()),
            // Read, never minted: pairing carries the state key this device
            // already uses. A device with none has no state folder to share,
            // and generating one here would hand the receiver a key to a log
            // that does not exist.
            state_key: config
                .general
                .state_sync
                .then(crate::credentials::state_secret)
                .flatten()
                .map(|s| crate::hex::encode(&s)),
        }
    }

    /// The AAD a payload is sealed under: its schema version (§8.1).
    pub fn aad(version: u32) -> Vec<u8> {
        format!("mailbox/pair/v{version}").into_bytes()
    }

    /// Serializes and seals.
    pub fn seal(&self, secret: &[u8], rng: &crate::Rng) -> Result<Vec<u8>> {
        let plaintext = serde_json::to_vec(self)?;
        Ok(crypto::seal(
            secret,
            &Payload::aad(self.version),
            &plaintext,
            rng,
        ))
    }

    /// Opens, checks freshness, and parses.
    pub fn open(secret: &[u8], sealed: &[u8], now: i64) -> Result<Payload> {
        let plaintext = crypto::open(secret, &Payload::aad(PAYLOAD_VERSION), sealed)?;
        let payload: Payload = serde_json::from_slice(&plaintext)
            .map_err(|e| Error::usage(format!("the pairing payload is malformed: {e}")))?;

        if payload.version != PAYLOAD_VERSION {
            return Err(Error::usage(format!(
                "this payload is version {} and this build speaks {PAYLOAD_VERSION}; \
                 upgrade the older device",
                payload.version
            )));
        }

        // §8.1: a receiver rejects a payload older than the TTL. The check is
        // two-sided — a payload from the future is a clock problem or a replay,
        // and accepting it would defeat the freshness window entirely.
        let age = now - payload.created;
        if age > crypto::FRESHNESS_SECONDS {
            return Err(Error::usage(format!(
                "this pairing payload is {age} seconds old; start the pairing again"
            )));
        }
        if age < -crypto::FRESHNESS_SECONDS {
            return Err(Error::usage(
                "this pairing payload is dated in the future; check the clocks on both devices",
            ));
        }
        Ok(payload)
    }

    /// A one-line summary for the confirmation prompt (§8.3).
    pub fn describe(&self) -> String {
        let identities: usize = self
            .accounts
            .iter()
            .map(|a| a.config.all_identities().len())
            .sum();
        format!(
            "{} account{}, {identities} identit{} from {}",
            self.accounts.len(),
            if self.accounts.len() == 1 { "" } else { "s" },
            if identities == 1 { "y" } else { "ies" },
            self.device,
        )
    }

    /// Applies the payload to a config (§8.3).
    ///
    /// Returns the new config rather than writing it: the caller decides where
    /// it goes, and a dry run should be able to see the result without one.
    pub fn apply(&self, existing: &Config, mode: ApplyMode) -> (Config, Applied) {
        let mut config = existing.clone();
        let mut applied = Applied::default();

        if mode == ApplyMode::Replace {
            applied.removed = config.accounts.keys().cloned().collect();
            config.accounts.clear();
        }

        for account in &self.accounts {
            // A colliding id is an update rather than a second account: two
            // devices calling the same mailbox `work` mean the same mailbox.
            if config.accounts.contains_key(&account.id) {
                applied.updated.push(account.id.clone());
            } else {
                applied.added.push(account.id.clone());
            }
            config
                .accounts
                .insert(account.id.clone(), account.config.clone());
            if account.credential.is_none() {
                applied.credentials_not_stored.push(account.id.clone());
            }
        }

        if let Some(preferences) = &self.preferences {
            config.general = preferences.clone();
        }
        (config, applied)
    }

    /// The environment-variable name a receiver can use for an account's
    /// credential when no keychain is available (§9).
    pub fn credential_env_name(account_id: &str) -> String {
        format!(
            "MAILBOX_PASSWORD_{}",
            account_id.to_ascii_uppercase().replace(['-', '.'], "_")
        )
    }
}

/// Shows the shape and never the secrets.
///
/// A redacting `Debug` rather than none: without one, every caller that wants
/// to `expect` on a `Result<Payload>` has to work around its absence, and the
/// obvious workaround is to add a derive. This way the type is usable and a
/// stray `{:?}` still cannot print a password.
impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payload")
            .field("version", &self.version)
            .field("created", &self.created)
            .field("device", &self.device)
            .field("accounts", &self.accounts)
            .field("state_key", &self.state_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl std::fmt::Debug for PairedAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairedAccount")
            .field("id", &self.id)
            .field("address", &self.config.address)
            .field("credential", &self.credential)
            .finish()
    }
}

impl std::fmt::Debug for PairedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The *kind* is worth showing — "this account paired without a
        // credential" is a thing a user needs to know — and the value never is.
        match self {
            PairedCredential::Password { .. } => f.write_str("Password(<redacted>)"),
            PairedCredential::OAuth2 { .. } => f.write_str("OAuth2(<redacted>)"),
        }
    }
}

/// The `mailbox://pair#…` link a QR carries (§8.1).
///
/// Everything the far side needs: the secret, and where to reach the device
/// waiting for it. The secret is in the *fragment*, which never leaves the
/// client in an HTTP request — belt and braces for a scheme that is never
/// fetched over HTTP anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingUri {
    pub secret: [u8; crypto::SECRET_BYTES],
    /// Where the receiving device is listening, for the LAN transport.
    pub address: Option<String>,
}

impl PairingUri {
    pub fn render(&self) -> String {
        let secret = crate::hex::encode(&self.secret);
        match &self.address {
            Some(address) => format!("mailbox://pair/{address}#{secret}"),
            None => format!("mailbox://pair#{secret}"),
        }
    }

    pub fn parse(text: &str) -> Result<PairingUri> {
        let bad = || Error::usage("that is not a mailbox pairing link");
        let rest = text.trim().strip_prefix("mailbox://pair").ok_or_else(bad)?;
        let (location, secret_hex) = rest.split_once('#').ok_or_else(bad)?;

        let bytes = crate::hex::decode(secret_hex.trim()).ok_or_else(bad)?;
        if bytes.len() != crypto::SECRET_BYTES {
            return Err(Error::usage(format!(
                "a pairing secret is {} bytes; this one is {}",
                crypto::SECRET_BYTES,
                bytes.len()
            )));
        }
        let mut secret = [0u8; crypto::SECRET_BYTES];
        secret.copy_from_slice(&bytes);

        let address = location.trim_start_matches('/').trim();
        Ok(PairingUri {
            secret,
            address: (!address.is_empty()).then(|| address.to_string()),
        })
    }
}

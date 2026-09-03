//! The encrypted export file (§8.3).
//!
//! > `mailbox pair --export-file backup.age` writes the same payload sealed
//! > with a passphrase for air-gapped moves; `mailbox pair --import-file` reads
//! > it.
//!
//! Same payload, same AEAD, different key source: a passphrase instead of a
//! 256-bit secret. That difference is the whole of the design here. A
//! passphrase is guessable, so it goes through Argon2id before it becomes a
//! key, and the parameters travel in the header so a file written today still
//! opens after they are raised.
//!
//! **This is not the age format.** The spec's `backup.age` is an illustrative
//! filename; what is written is a `mailbox-pair` container, and the header says
//! so in plain text so nobody mistakes one for the other.

use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::pair::{crypto, Payload};

/// The container's magic. First line of the file, human-readable, so `head`
/// answers "what is this".
pub const MAGIC: &str = "mailbox-pair/v1";

/// Argon2id parameters.
///
/// Interactive rather than paranoid: this guards a file the user is moving
/// between their own machines, and a KDF that takes ten seconds on a phone is
/// one people work around. 64 MiB and three passes is the OWASP interactive
/// recommendation.
const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;
const LANES: u32 = 1;

/// The clear-text header. Everything needed to derive the key again, and
/// nothing that helps without the passphrase.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    format: String,
    /// The payload schema version, also used as AEAD associated data.
    payload_version: u32,
    kdf: String,
    memory_kib: u32,
    passes: u32,
    lanes: u32,
    /// Hex. Unique per file, so two exports of the same accounts under the same
    /// passphrase share no key.
    salt: String,
}

/// Writes a payload sealed under a passphrase.
pub fn export(payload: &Payload, passphrase: &str, rng: &crate::Rng) -> Result<Vec<u8>> {
    if passphrase.trim().is_empty() {
        // An empty passphrase would produce a file that looks encrypted and is
        // not, which is worse than refusing.
        return Err(Error::usage("an export needs a passphrase"));
    }

    let salt = rng.bytes::<16>();
    let key = derive_key(passphrase, &salt)?;
    let plaintext = serde_json::to_vec(payload)?;
    let sealed = crypto::seal(&key, &Payload::aad(payload.version), &plaintext, rng);

    let header = Header {
        format: MAGIC.to_string(),
        payload_version: payload.version,
        kdf: "argon2id".to_string(),
        memory_kib: MEMORY_KIB,
        passes: PASSES,
        lanes: LANES,
        salt: crate::hex::encode(&salt),
    };

    // One JSON header line, then the sealed bytes as base64. Line-oriented so
    // the file survives being pasted into a chat window, which is exactly what
    // someone moving a config between machines will do.
    use base64::Engine;
    let mut out = serde_json::to_vec(&header)?;
    out.push(b'\n');
    out.extend_from_slice(
        base64::engine::general_purpose::STANDARD
            .encode(&sealed)
            .as_bytes(),
    );
    out.push(b'\n');
    Ok(out)
}

/// Reads a payload back.
pub fn import(bytes: &[u8], passphrase: &str, now: i64) -> Result<Payload> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| Error::usage("this file is empty"))?;
    let header: Header = serde_json::from_str(header_line)
        .map_err(|_| Error::usage("this is not a mailbox pairing export"))?;
    if header.format != MAGIC {
        return Err(Error::usage(format!(
            "this file says it is {:?}; this build reads {MAGIC}",
            header.format
        )));
    }

    let salt = crate::hex::decode(&header.salt)
        .ok_or_else(|| Error::usage("the export header is malformed"))?;

    // The header's parameters, not today's constants: a file written before
    // they were raised must still open.
    let key = derive_key_with(
        passphrase,
        &salt,
        header.memory_kib,
        header.passes,
        header.lanes,
    )?;

    use base64::Engine;
    let body: String = lines.collect::<Vec<_>>().join("");
    let sealed = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|_| Error::usage("the export body is malformed"))?;

    let plaintext = crypto::open(&key, &Payload::aad(header.payload_version), &sealed)
        .map_err(|_| Error::usage("wrong passphrase, or the file has been altered"))?;

    let payload: Payload = serde_json::from_slice(&plaintext)
        .map_err(|e| Error::usage(format!("the payload is malformed: {e}")))?;

    // §8.1's freshness applies to a *channel*, not to a file someone
    // deliberately kept: an export is for air-gapped moves and may sit on a USB
    // stick for a week. The timestamp is still reported so the user can see how
    // old it is.
    let _ = now;
    Ok(payload)
}

/// How old an exported payload is, in days, for the caller to report.
pub fn age_days(payload: &Payload, now: i64) -> i64 {
    (now - payload.created).max(0) / 86_400
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32]> {
    derive_key_with(passphrase, salt, MEMORY_KIB, PASSES, LANES)
}

fn derive_key_with(
    passphrase: &str,
    salt: &[u8],
    memory_kib: u32,
    passes: u32,
    lanes: u32,
) -> Result<[u8; 32]> {
    // Bounded, so a malicious header cannot ask for a terabyte of RAM and turn
    // opening a file into a denial of service.
    if memory_kib > 1024 * 1024 || passes > 32 || lanes > 16 {
        return Err(Error::usage(
            "this export asks for implausible key-derivation parameters",
        ));
    }

    let params = Params::new(memory_kib, passes, lanes, Some(32))
        .map_err(|e| Error::usage(format!("bad key-derivation parameters: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = [0u8; 32];
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| Error::usage(format!("key derivation failed: {e}")))?;
    Ok(key)
}

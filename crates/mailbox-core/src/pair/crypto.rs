//! The pairing cryptography of §8.1.
//!
//! ```text
//! S    = 256 bits of CSPRNG output, carried out of band (a QR, a spoken code)
//! cid  = HKDF(S, "channel")
//! k    = HKDF(S, "key")
//! seal = XChaCha20-Poly1305(k, payload)   AAD = the payload's schema version
//! SAS  = HKDF(S, "sas" ‖ transcript)      shown on both devices
//! ```
//!
//! Everything here is a named primitive from a reviewed crate — HKDF-SHA256 and
//! XChaCha20-Poly1305 — composed in the shape §8.1 specifies. Nothing invents a
//! construction. What this module owns is the *labels*, and those are the part
//! that has to be pinned: a change to an info string silently makes two versions
//! of mailbox unable to pair, so every one of them has a known-answer vector.

use chacha20poly1305::aead::{Aead, KeyInit, Payload as AeadPayload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::error::{Error, Result};

/// The pairing secret: 256 bits, carried entirely out of band (§8.1).
///
/// Long enough that it is the authentication — an attacker who does not have it
/// cannot produce or read a payload — which is why the QR and LAN transports
/// need no password exchange at all.
pub const SECRET_BYTES: usize = 32;

/// XChaCha20-Poly1305's nonce. Extended-nonce precisely so a random one per
/// message is safe without a counter to keep.
pub const NONCE_BYTES: usize = 24;

/// §8.1: channels live at most 300 s, and a receiver rejects a payload older
/// than the TTL.
pub const FRESHNESS_SECONDS: i64 = 300;

/// The info strings. Pinned by test, because changing one is a silent
/// incompatibility rather than a visible error.
pub const INFO_CHANNEL: &[u8] = b"mailbox/pair/channel/v1";
pub const INFO_KEY: &[u8] = b"mailbox/pair/key/v1";
pub const INFO_SAS: &[u8] = b"mailbox/pair/sas/v1";

/// A freshly minted pairing secret.
pub fn generate_secret(rng: &crate::Rng) -> [u8; SECRET_BYTES] {
    rng.bytes::<SECRET_BYTES>()
}

/// `HKDF-SHA256(ikm = secret, info)`, expanded to `N` bytes.
///
/// No salt: §8.1 names none, and the secret is already 256 bits of CSPRNG
/// output rather than a low-entropy password that would need one.
pub fn derive<const N: usize>(secret: &[u8], info: &[u8]) -> [u8; N] {
    let hkdf = Hkdf::<Sha256>::new(None, secret);
    let mut out = [0u8; N];
    hkdf.expand(info, &mut out)
        .expect("HKDF-SHA256 cannot fail for outputs under 255 blocks");
    out
}

/// The channel id: what a relay would see, and all it would see (§8.2).
pub fn channel_id(secret: &[u8]) -> String {
    crate::hex::encode(&derive::<16>(secret, INFO_CHANNEL))
}

/// The payload encryption key.
pub fn payload_key(secret: &[u8]) -> [u8; 32] {
    derive::<32>(secret, INFO_KEY)
}

/// Seals a payload under the secret.
///
/// `aad` is the payload's schema version (§8.1), so a payload cannot be
/// replayed against a build that would interpret its fields differently — the
/// version is authenticated even though it travels in the clear.
pub fn seal(secret: &[u8], aad: &[u8], plaintext: &[u8], rng: &crate::Rng) -> Vec<u8> {
    seal_with_key(&payload_key(secret), aad, plaintext, rng)
}

/// [`seal`], for a caller that derived its own key.
///
/// §5.4's state records are sealed with the same AEAD under a key derived from
/// a different secret with a different label ([`crate::state::wire`]). Sharing
/// the construction rather than writing a second one is the point: there is
/// one place where nonces are chosen and AAD is bound, and it is this one.
pub fn seal_with_key(key: &[u8; 32], aad: &[u8], plaintext: &[u8], rng: &crate::Rng) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce_bytes = rng.bytes::<NONCE_BYTES>();
    let nonce = XNonce::from(nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            &nonce,
            AeadPayload {
                msg: plaintext,
                aad,
            },
        )
        .expect("XChaCha20-Poly1305 encryption is infallible for in-memory input");

    // Nonce first, so the sealed blob is self-contained.
    let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    out
}

/// Opens a sealed payload.
///
/// Every failure is the same error on purpose. A wrong key, a truncated blob,
/// a tampered byte, and a mismatched version are all "this did not
/// authenticate", and distinguishing them for the caller would hand an attacker
/// an oracle.
pub fn open(secret: &[u8], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    open_with_key(&payload_key(secret), aad, sealed)
}

/// [`open`], for a caller that derived its own key. Fails the same single way.
pub fn open_with_key(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() <= NONCE_BYTES {
        return Err(Error::usage("the pairing payload did not authenticate"));
    }
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_BYTES);
    let cipher = XChaCha20Poly1305::new(key.into());

    cipher
        .decrypt(
            &XNonce::try_from(nonce_bytes)
                .map_err(|_| Error::usage("the pairing payload did not authenticate"))?,
            AeadPayload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error::usage("the pairing payload did not authenticate"))
}

/// Words for the short authentication string.
///
/// Chosen to survive being read aloud over a phone: no homophones, no pairs
/// that differ by one letter, nothing that sounds like a letter of the
/// alphabet. 64 of them, so three words carry 18 bits.
const SAS_WORDS: &[&str] = &[
    "amber", "anchor", "apple", "arrow", "bacon", "banjo", "barrel", "bishop", "bronze", "cactus",
    "candle", "canyon", "cargo", "cobra", "comet", "copper", "coral", "cousin", "crayon",
    "dolphin", "dragon", "ember", "falcon", "fossil", "garlic", "gecko", "ginger", "granite",
    "harbor", "hazel", "helmet", "iceberg", "igloo", "ivory", "jacket", "jungle", "kettle",
    "lantern", "lemon", "lizard", "magnet", "mango", "marble", "meadow", "mint", "nectar",
    "nickel", "orchid", "otter", "pepper", "pigeon", "planet", "pumpkin", "quartz", "rabbit",
    "ribbon", "saffron", "silver", "tiger", "tunnel", "velvet", "walnut", "willow", "zebra",
];

/// The short authentication string both devices display (§8.1, §8.3).
///
/// `amber-tiger-04-mint`. Derived from the secret *and the transcript*, so it
/// says more than "we both know the secret": it says we both saw the same
/// conversation. That is what makes a mismatch mean an active attacker rather
/// than a typo.
pub fn short_authentication_string(secret: &[u8], transcript: &[u8]) -> String {
    let mut info = Vec::with_capacity(INFO_SAS.len() + 1 + transcript.len());
    info.extend_from_slice(INFO_SAS);
    info.push(0x1f);
    info.extend_from_slice(transcript);

    let bytes = derive::<8>(secret, &info);
    let n = SAS_WORDS.len() as u32;
    let pick =
        |i: usize| SAS_WORDS[(u32::from_be_bytes([0, 0, bytes[i], bytes[i + 1]]) % n) as usize];

    format!(
        "{}-{}-{:02}-{}",
        pick(0),
        pick(2),
        u16::from_be_bytes([bytes[4], bytes[5]]) % 100,
        pick(6),
    )
}

/// The transcript a SAS is computed over.
///
/// Both sides build this from what they actually saw, so any difference — a
/// substituted payload, a replayed channel, a different peer — changes the
/// words. Ordered and length-prefixed so two different conversations cannot
/// concatenate to the same bytes.
pub fn transcript(channel: &str, sealed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(channel.len() + sealed.len() + 16);
    out.extend_from_slice(&(channel.len() as u32).to_be_bytes());
    out.extend_from_slice(channel.as_bytes());
    out.extend_from_slice(&(sealed.len() as u32).to_be_bytes());
    out.extend_from_slice(sealed);
    out
}

/// Constant-time comparison, for anything an attacker could probe by timing.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

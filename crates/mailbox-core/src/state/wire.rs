//! One state record as a MIME message, sealed (§5.4).
//!
//! > Each record is a small MIME message; devices compact the folder
//! > periodically. The encryption key is part of the pairing payload (§8), so
//! > a paired device can read the state and a server operator cannot.
//!
//! ## What the server operator sees
//!
//! A folder of small messages, all alike:
//!
//! ```text
//! Message-ID: <9f3c…@mailbox.state>
//! Subject: mailbox state
//! X-Mailbox-State: 1
//! Date: <the day it was written>
//! Content-Type: application/octet-stream
//! Content-Transfer-Encoding: base64
//!
//! <nonce ‖ ciphertext>
//! ```
//!
//! and nothing else. Everything in [`Record`] is inside the sealed body —
//! including the device id, which in a header would quietly tell the operator
//! how many machines the user owns and when each of them was last awake. The
//! headers carry only what IMAP needs to store the message and what this code
//! needs to recognise its own records before spending a decryption on them.
//!
//! `Date` is a day, not a second: a per-record timestamp in the clear is a
//! record of when the user tags mail, and the sealed `at` is the one the fold
//! actually reads.
//!
//! ## The key
//!
//! Derived from the account's 32-byte state secret with the same HKDF and the
//! same AEAD as pairing ([`crate::pair::crypto`]) — a separate label, so the
//! two keys are unrelated, and no second cryptographic construction to review.
//! The secret itself is a credential: it lives where credentials live
//! ([`crate::credentials`]) and never in the config file.
//!
//! A record that does not authenticate is **skipped, not fatal**. After a
//! revocation rotates the key (§8.3) the folder still holds every record
//! written under the old one, and a device that treated those as corruption
//! would refuse to sync a mailbox that is working exactly as designed.

use crate::error::{Error, Result};
use crate::state::Record;

/// The record schema version, authenticated as AAD.
///
/// A reader that does not know a version skips the record rather than guessing
/// at it — the same rule as the pairing payload, for the same reason: a
/// half-understood assertion is worse than a missing one.
pub const VERSION: u32 = 1;

/// The header that marks one of ours, so a folder someone else has written to
/// costs a header check rather than a decryption per message.
pub const VERSION_HEADER: &str = "X-Mailbox-State";

/// HKDF label for the state key. Distinct from every label in
/// [`crate::pair::crypto`], so the pairing secret and the state secret cannot
/// be confused for one another even if a caller passes the wrong one.
pub const INFO_STATE: &[u8] = b"mailbox/state/key/v1";

/// The folder the log lives in (§5.4's default, configurable).
pub const DEFAULT_FOLDER: &str = "Mailbox/.state";

/// The state secret: 32 bytes, carried in the pairing payload (§8).
pub const SECRET_BYTES: usize = 32;

/// A freshly minted state secret, for an account that has none yet.
pub fn generate_secret(rng: &crate::Rng) -> [u8; SECRET_BYTES] {
    rng.bytes::<SECRET_BYTES>()
}

/// The AEAD key for a state secret.
fn key(secret: &[u8]) -> [u8; 32] {
    crate::pair::crypto::derive::<32>(secret, INFO_STATE)
}

/// Renders a record as the message that goes in the folder.
pub fn seal(
    record: &Record,
    secret: &[u8],
    clock: &crate::Clock,
    rng: &crate::Rng,
) -> Result<Vec<u8>> {
    let plaintext = serde_json::to_vec(record)
        .map_err(|e| Error::usage(format!("a state record would not serialize: {e}")))?;
    let sealed =
        crate::pair::crypto::seal_with_key(&key(secret), &VERSION.to_be_bytes(), &plaintext, rng);

    let mut out = String::new();
    out.push_str(&format!("Message-ID: <{}@mailbox.state>\r\n", rng.hex(16)));
    out.push_str("Subject: mailbox state\r\n");
    out.push_str(&format!("{VERSION_HEADER}: {VERSION}\r\n"));
    out.push_str(&format!("Date: {}\r\n", day(clock.now())));
    out.push_str("Content-Type: application/octet-stream\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n");
    out.push_str("\r\n");
    for line in base64(&sealed).as_bytes().chunks(76) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push_str("\r\n");
    }
    Ok(out.into_bytes())
}

/// Why a message in the folder yielded no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Not one of ours, or a version this build does not know.
    NotOurs,
    /// Ours by its header, but it did not authenticate — the ordinary state of
    /// every record written before the last key rotation (§8.3).
    WrongKey,
    /// It authenticated and the plaintext was not a record. A bug on some
    /// device, or a build from the future; either way, skip it.
    Unreadable,
}

/// Reads one message back into a record.
///
/// `Err` is never "this mailbox is broken" — it is one message that this
/// device cannot use, and the caller goes on to the next.
pub fn open(message: &[u8], secret: &[u8]) -> std::result::Result<Record, Skipped> {
    let text = String::from_utf8_lossy(message);
    let (headers, body) = split(&text).ok_or(Skipped::NotOurs)?;

    let version = header(headers, VERSION_HEADER).ok_or(Skipped::NotOurs)?;
    if version.trim().parse::<u32>() != Ok(VERSION) {
        return Err(Skipped::NotOurs);
    }

    let sealed = unbase64(body).ok_or(Skipped::Unreadable)?;
    let plaintext =
        crate::pair::crypto::open_with_key(&key(secret), &VERSION.to_be_bytes(), &sealed)
            .map_err(|_| Skipped::WrongKey)?;
    serde_json::from_slice(&plaintext).map_err(|_| Skipped::Unreadable)
}

/// Splits headers from body at the first blank line.
fn split(text: &str) -> Option<(&str, &str)> {
    let at = text
        .find("\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| text.find("\n\n").map(|i| (i, 2)))?;
    Some((&text[..at.0], &text[at.0 + at.1..]))
}

fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// RFC 5322 `Date`, to the day. See the module docs for why not the second.
fn day(now: i64) -> String {
    let days = now.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{:02} {} {} 00:00:00 +0000", d, MONTHS[(m - 1) as usize], y)
}

/// Howard Hinnant's civil-from-days, which is the standard way to do this
/// without a calendar dependency for one header.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn unbase64(input: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in input.bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Entry;

    fn a_record() -> Record {
        Record {
            id: "0123456789abcdef".into(),
            device: "laptop".into(),
            seq: 3,
            at: 1_757_000_000,
            entry: Entry::Tag {
                message: "abc1234".into(),
                tag: "work".into(),
                present: true,
            },
        }
    }

    fn fixtures() -> ([u8; 32], crate::Clock, crate::Rng) {
        (
            [7u8; 32],
            crate::Clock::fixed(1_757_000_000),
            crate::Rng::seeded(b"state-wire"),
        )
    }

    #[test]
    fn a_record_survives_the_round_trip() {
        let (secret, clock, rng) = fixtures();
        let message = seal(&a_record(), &secret, &clock, &rng).expect("seal");
        assert_eq!(open(&message, &secret).expect("open"), a_record());
    }

    /// The point of the whole exercise: what the operator can read.
    #[test]
    fn nothing_in_the_clear_names_the_device_or_the_message() {
        let (secret, clock, rng) = fixtures();
        let message = seal(&a_record(), &secret, &clock, &rng).expect("seal");
        let text = String::from_utf8_lossy(&message);
        let (headers, _) = split(&text).expect("a message");
        for secret_ish in ["laptop", "abc1234", "work"] {
            assert!(
                !headers.contains(secret_ish),
                "`{secret_ish}` leaked into the headers:\n{headers}"
            );
        }
    }

    /// After a revocation rotates the key, every older record fails to open.
    /// That is the normal state of the folder, not an error.
    #[test]
    fn a_record_under_another_key_is_skipped_not_fatal() {
        let (secret, clock, rng) = fixtures();
        let message = seal(&a_record(), &secret, &clock, &rng).expect("seal");
        assert_eq!(open(&message, &[9u8; 32]), Err(Skipped::WrongKey));
    }

    #[test]
    fn a_tampered_body_does_not_authenticate() {
        let (secret, clock, rng) = fixtures();
        let mut message = seal(&a_record(), &secret, &clock, &rng).expect("seal");
        let last = message.len() - 6;
        message[last] ^= 0x01;
        assert!(matches!(
            open(&message, &secret),
            Err(Skipped::WrongKey) | Err(Skipped::Unreadable)
        ));
    }

    #[test]
    fn somebody_elses_message_in_the_folder_is_not_ours() {
        let message = b"Subject: hello\r\nFrom: a@b\r\n\r\nnot a state record\r\n";
        assert_eq!(open(message, &[7u8; 32]), Err(Skipped::NotOurs));
    }

    #[test]
    fn a_future_version_is_skipped_rather_than_guessed_at() {
        let message = format!("Subject: mailbox state\r\n{VERSION_HEADER}: 99\r\n\r\nAAAA\r\n");
        assert_eq!(open(message.as_bytes(), &[7u8; 32]), Err(Skipped::NotOurs));
    }

    #[test]
    fn the_date_header_is_a_day_not_a_second() {
        assert_eq!(day(1_757_000_000), "04 Sep 2025 00:00:00 +0000");
        assert_eq!(day(0), "01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn base64_round_trips_every_length_of_tail() {
        for n in 0..8usize {
            let data: Vec<u8> = (0..n as u8).collect();
            assert_eq!(unbase64(&base64(&data)).expect("decode"), data, "n={n}");
        }
    }

    /// The state key must not be the pairing key, even from the same secret.
    #[test]
    fn the_state_key_is_not_the_pairing_key() {
        let secret = [7u8; 32];
        assert_ne!(key(&secret), crate::pair::crypto::payload_key(&secret));
    }

    /// The label is a wire format: changing it makes two builds unable to read
    /// each other's state, silently. Pinned like the pairing labels are.
    #[test]
    fn the_hkdf_label_is_pinned() {
        assert_eq!(INFO_STATE, b"mailbox/state/key/v1");
        assert_eq!(
            crate::hex::encode(&key(&[0u8; 32])),
            crate::hex::encode(&crate::pair::crypto::derive::<32>(&[0u8; 32], INFO_STATE))
        );
    }
}

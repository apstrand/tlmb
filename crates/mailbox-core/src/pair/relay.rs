//! The rendezvous relay, client side (§8.2).
//!
//! > Deliberately trivial, so that running your own is a five-minute job and
//! > trusting the default one is a small decision.
//!
//! Two devices that are not on the same network need somewhere to leave three
//! short messages for each other: the two PAKE messages of §8.1, and then the
//! sealed payload. That is the entire job. The relay never holds a key, never
//! sees a plaintext, and keeps nothing after a message is collected.
//!
//! ## The channel id is public, and that is a correction
//!
//! §8.1 originally derived the channel from the pairing secret —
//! `cid = HKDF(S, "channel")`. That is sound for the QR and LAN transports,
//! where `S` is 256 bits. On the short-code path it is a break: `S` is a
//! 45-bit code, so anyone who saw the channel id — **the relay operator most
//! of all** — could try every code offline and recover it in minutes. The
//! PAKE would then be protecting nothing, since its whole claim is that an
//! attacker learns nothing offline.
//!
//! So the rendezvous identifier is not derived from the code at all. The relay
//! hands out a short **slot** that is public by construction, and the code the
//! user reads out is `slot-secret`:
//!
//! ```text
//!     7-K7M-4QB-XZ2
//!     ^ ^^^^^^^^^^^
//!     |  the PAKE password: 45 bits, never sent anywhere
//!     the slot: public, routing only, tells the relay which shelf
//! ```
//!
//! The slot is not a secret and is not treated as one. Knowing it lets an
//! attacker interfere with a pairing — which is what the PAKE and the short
//! authentication string are there to catch — and never lets them read one.
//!
//! ## What the relay can do to you
//!
//! It can refuse service, and it can watch two IP addresses exchange three
//! opaque blobs. It cannot read the payload (sealed under a key derived from a
//! code it never sees), cannot substitute one (it has no key to seal with, and
//! a substitution changes the transcript, which changes the words both devices
//! display), and cannot brute-force the code (one online guess per slot, and
//! the slot is consumed).
//!
//! That is why §2 can call it optional and self-hostable without hedging: a
//! hostile relay is a denial of service, not a disclosure.

use std::time::Duration;

use crate::error::{Error, Result};

/// §8.1: channels live at most 300 s.
pub const TTL_SECONDS: u64 = crate::pair::crypto::FRESHNESS_SECONDS as u64;

/// §8.2's ceiling on one message.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// How long a device waits for the other one before giving up.
const WAIT: Duration = Duration::from_secs(TTL_SECONDS);

/// How often to ask. Long enough not to hammer a shared relay, short enough
/// that pairing does not feel broken.
const POLL: Duration = Duration::from_millis(700);

/// The three things a slot carries, in order.
///
/// Named rather than numbered so a relay's logs — which are the only thing it
/// legitimately has — say what stage a stuck pairing reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Box_ {
    /// The initiator's CPace message.
    Offer,
    /// The responder's CPace message.
    Answer,
    /// The sealed payload.
    Payload,
}

impl Box_ {
    pub fn name(self) -> &'static str {
        match self {
            Box_::Offer => "offer",
            Box_::Answer => "answer",
            Box_::Payload => "payload",
        }
    }

    pub fn parse(name: &str) -> Option<Box_> {
        match name {
            "offer" => Some(Box_::Offer),
            "answer" => Some(Box_::Answer),
            "payload" => Some(Box_::Payload),
            _ => None,
        }
    }

    pub fn all() -> [Box_; 3] {
        [Box_::Offer, Box_::Answer, Box_::Payload]
    }
}

/// A relay, addressed by its base URL.
#[derive(Debug, Clone)]
pub struct Relay {
    base: String,
}

impl Relay {
    /// `https://pair.example` — trailing slashes are tolerated.
    ///
    /// Plaintext is refused for anything but loopback, by the same rule as
    /// every other connection this program makes (§9) — enforced in
    /// [`crate::https`], which is where that rule lives.
    pub fn new(base: &str) -> Relay {
        Relay {
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Asks the relay for a slot.
    pub fn allocate(&self) -> Result<String> {
        let response = crate::https::post_bytes(&format!("{}/a", self.base), b"")?;
        if response.status != 201 && response.status != 200 {
            return Err(relay_error(response.status, "asking for a slot"));
        }
        let value = response.json()?;
        let slot = value
            .get("slot")
            .and_then(|s| s.as_str())
            .ok_or_else(|| Error::Network("the relay did not name a slot".into()))?;
        validate_slot(slot)?;
        Ok(slot.to_string())
    }

    /// Leaves a message. `409` means someone already used that box, which is
    /// §8.2's burn: a slot carries each message once.
    pub fn put(&self, slot: &str, which: Box_, body: &[u8]) -> Result<()> {
        validate_slot(slot)?;
        if body.len() > MAX_MESSAGE {
            return Err(Error::usage(format!(
                "a relay message is at most {MAX_MESSAGE} bytes; this one is {}",
                body.len()
            )));
        }
        let response = crate::https::post_bytes(&self.url(slot, which), body)?;
        match response.status {
            201 | 200 => Ok(()),
            409 => Err(Error::Network(format!(
                "that pairing slot has already been used for the {}; start again",
                which.name()
            ))),
            status => Err(relay_error(status, "leaving a message")),
        }
    }

    /// Collects a message, waiting for it to appear.
    ///
    /// The relay deletes on read, so this succeeds exactly once per box. A
    /// `404` is "not there *yet*" as often as "never was", and the two are
    /// indistinguishable from here — which is why the wait is bounded by the
    /// same TTL the relay enforces rather than by a guess.
    pub fn take(&self, slot: &str, which: Box_, clock: &crate::Clock) -> Result<Vec<u8>> {
        validate_slot(slot)?;
        let url = self.url(slot, which);
        let deadline = clock.now() + WAIT.as_secs() as i64;
        loop {
            let response = crate::https::get(&url)?;
            match response.status {
                200 => return Ok(response.body),
                404 => {}
                status => return Err(relay_error(status, "collecting a message")),
            }
            if clock.now() >= deadline {
                return Err(Error::Timeout(WAIT.as_secs()));
            }
            std::thread::sleep(POLL);
        }
    }

    /// One collection attempt, without waiting. For a caller with its own loop.
    pub fn try_take(&self, slot: &str, which: Box_) -> Result<Option<Vec<u8>>> {
        validate_slot(slot)?;
        let response = crate::https::get(&self.url(slot, which))?;
        match response.status {
            200 => Ok(Some(response.body)),
            404 => Ok(None),
            status => Err(relay_error(status, "collecting a message")),
        }
    }

    fn url(&self, slot: &str, which: Box_) -> String {
        format!("{}/r/{}.{}", self.base, slot, which.name())
    }
}

/// A relay's failure, in words that say whose problem it is.
fn relay_error(status: u16, doing: &str) -> Error {
    match status {
        429 => Error::Network(
            "the relay is rate-limiting this address; wait a moment and start again".into(),
        ),
        413 => Error::Network("the relay refused the message as too large".into()),
        500..=599 => Error::Network(format!("the relay failed while {doing} (HTTP {status})")),
        _ => Error::Network(format!("the relay answered HTTP {status} while {doing}")),
    }
}

/// Slots are short, public, and go in a URL path.
///
/// Validated on both sides rather than trusted: a slot arrives from a relay's
/// reply *and* from a code a person typed, and neither is a reason to put
/// arbitrary text into a request line.
pub fn validate_slot(slot: &str) -> Result<()> {
    if slot.is_empty() || slot.len() > 16 {
        return Err(Error::usage(
            "a pairing slot is between 1 and 16 characters",
        ));
    }
    if !slot.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Error::usage("a pairing slot is letters and digits only"));
    }
    Ok(())
}

/// A full pairing code: the public slot and the secret half.
///
/// Rendered as `7-K7M-4QB-XZ2` and read back leniently, because it is typed by
/// a person reading it off another screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    pub slot: String,
    /// The normalized nine-character secret ([`crate::pair::pake`]).
    pub secret: String,
}

impl Code {
    pub fn render(&self) -> String {
        let grouped = self
            .secret
            .as_bytes()
            .chunks(3)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("-");
        format!("{}-{}", self.slot, grouped)
    }

    /// Parses what the user typed.
    ///
    /// The slot is everything before the first separator, and the rest is the
    /// secret — which means a slot can grow past one character without the
    /// format changing.
    pub fn parse(input: &str) -> Result<Code> {
        // Separators are normalized *before* the split, not stripped: someone
        // who reads the code aloud and someone who copies it will type
        // `7 K7M 4QB XZ2` and `7-K7M-4QB-XZ2`, and stripping first would erase
        // the boundary that says where the slot ends.
        let cleaned: String = input
            .trim()
            .chars()
            .map(|c| {
                if c.is_whitespace() || c == '_' {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        let cleaned = cleaned.trim_matches('-');
        let (slot, rest) = cleaned.split_once('-').ok_or_else(|| {
            Error::usage(
                "a pairing code looks like `7-K7M-4QB-XZ2`: the slot, then nine characters",
            )
        })?;
        validate_slot(slot)?;
        let secret = crate::pair::pake::normalize_code(rest)?;
        Ok(Code {
            slot: slot.to_string(),
            secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_round_trips() {
        let code = Code {
            slot: "7".into(),
            secret: "K7M4QBXZ2".into(),
        };
        assert_eq!(code.render(), "7-K7M-4QB-XZ2");
        assert_eq!(Code::parse("7-K7M-4QB-XZ2").expect("parse"), code);
    }

    #[test]
    fn a_code_is_read_the_way_a_person_types_it() {
        let expected = Code {
            slot: "7".into(),
            secret: "K7M4QBXZ2".into(),
        };
        for typed in [
            "7-k7m-4qb-xz2",
            "7 K7M 4QB XZ2",
            "7-K7M4QBXZ2",
            "  7-k7m-4qb-xz2  ",
        ] {
            assert_eq!(Code::parse(typed).expect(typed), expected, "{typed}");
        }
        // Crockford's substitutions still apply to the secret half.
        assert_eq!(Code::parse("7-O12-345-678").expect("O").secret, "012345678");
    }

    #[test]
    fn a_multi_character_slot_works() {
        let code = Code::parse("42B-K7M-4QB-XZ2").expect("parse");
        assert_eq!(code.slot, "42B");
        assert_eq!(code.secret, "K7M4QBXZ2");
    }

    #[test]
    fn a_code_that_is_not_one_is_refused() {
        assert!(Code::parse("K7M4QBXZ2").is_err(), "no slot");
        assert!(Code::parse("7-K7M-4QB").is_err(), "short secret");
        assert!(Code::parse("7-K7M-4QB-XZ!").is_err(), "bad character");
        assert!(Code::parse("").is_err());
    }

    /// A slot goes into a URL path. Anything that could steer a request
    /// somewhere else is refused before it is ever formatted in.
    #[test]
    fn a_slot_cannot_smuggle_a_path() {
        for bad in ["../x", "a/b", "a?b", "a#b", "", "a b"] {
            assert!(validate_slot(bad).is_err(), "{bad:?} should be refused");
        }
        for good in ["7", "42B", "abcDEF123"] {
            assert!(validate_slot(good).is_ok(), "{good:?} should be allowed");
        }
    }

    #[test]
    fn the_boxes_name_themselves_consistently() {
        for b in Box_::all() {
            assert_eq!(Box_::parse(b.name()), Some(b));
        }
        assert_eq!(Box_::parse("something-else"), None);
    }

    #[test]
    fn the_url_is_the_shape_the_spec_documents() {
        let relay = Relay::new("https://pair.example/");
        assert_eq!(relay.base(), "https://pair.example");
        assert_eq!(
            relay.url("7", Box_::Offer),
            "https://pair.example/r/7.offer"
        );
    }

    #[test]
    fn an_oversized_message_is_refused_before_it_is_sent() {
        let relay = Relay::new("http://127.0.0.1:1");
        let err = relay
            .put("7", Box_::Payload, &vec![0u8; MAX_MESSAGE + 1])
            .expect_err("too large");
        assert!(err.to_string().contains("at most"), "{err}");
    }
}

//! The short-code path of §8.1: a typeable code, and the PAKE that makes it
//! safe to type.
//!
//! > a 9-character human-typable code (Crockford base32, no ambiguous glyphs,
//! > ~45 bits) is too weak to be a key, so it is used as the password in a
//! > balanced PAKE (CPace, X25519 group). […] an attacker gets one online
//! > guess per channel and learns nothing offline.
//!
//! ## Why a PAKE and not just a shorter secret
//!
//! The QR and LAN transports carry 256 bits, and that secret *is* the
//! authentication: whoever holds it can seal and open a payload. Forty-five
//! bits cannot do that job. Used directly as a key it would be a credential
//! leak with extra steps — anyone who captured the exchange could try every
//! code offline, at a few billion per second, and be done in under a day.
//!
//! A balanced PAKE changes the arithmetic completely. The code never becomes a
//! key; it selects a group generator, and each side sends a blinded point
//! derived from it. Two parties who used the same code arrive at the same
//! 256-bit secret. Two parties who did not arrive at *unrelated* secrets, and
//! neither the exchange nor the result tells anyone which code was tried. So
//! an attacker gets one guess per handshake, online, against a channel that
//! then burns — and 2^45 online guesses is not an attack, it is a lifetime.
//!
//! ## How a wrong code is discovered
//!
//! Not here. A CPace handshake with mismatched passwords **succeeds**, and
//! leaves the two sides holding different secrets. That is the property doing
//! the work: an implementation that could tell you the password was wrong
//! could also be asked, and would be an offline oracle.
//!
//! The mismatch surfaces one step later, when the sealed payload fails to
//! authenticate under a secret the other side does not share — and, before
//! that, when the two devices display different short authentication strings
//! (§8.1) and the user says so. Both are what a caller must do; neither is
//! optional.
//!
//! ## What this is not
//!
//! **It has not had the outside review §16 asks for.** That gate was lifted
//! deliberately, not met. What is here is a reputable implementation of a
//! standard protocol — a port of jedisct1's libsodium CPace, over Ristretto255
//! — driven through the shape §8.1 describes, with the composition tested. It
//! is not a reviewed system, and the difference matters most precisely where
//! this is used: the payload it protects is every credential the user owns.
//! See `TODO.md` for what a review would still need to cover.
//!
//! One deviation from §8.1's wording: the group is **Ristretto255**, not raw
//! X25519. Ristretto is a prime-order group built on the same curve, which
//! removes the cofactor handling that small-subgroup mistakes live in. Every
//! CPace implementation worth using makes the same choice.

use crate::error::{Error, Result};
use crate::pair::crypto;

/// Crockford base32: no `I`, `L`, `O`, or `U`. The first three because they
/// are read back as `1`, `1`, and `0`; the last because its absence keeps a
/// randomly generated code from spelling something the user would rather not
/// read aloud.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// §8.1's nine characters — 45 bits at five bits each.
pub const CODE_CHARS: usize = 9;

/// How the code is shown: `XXX-XXX-XXX`. Groups of three are what people can
/// hold in their head between glancing at one screen and typing at another.
const GROUP: usize = 3;

/// The identities CPace binds into the generator, so a handshake meant for
/// this protocol cannot be replayed into another one.
const ID_INITIATOR: &str = "mailbox/pair/initiator/v1";
const ID_RESPONDER: &str = "mailbox/pair/responder/v1";

/// Associated data, authenticated by the handshake.
const AD: &[u8] = b"mailbox/pair/pake/v1";

/// HKDF label turning CPace's two directional keys into the one 256-bit
/// secret the rest of §8 already knows how to use.
const INFO_SECRET: &[u8] = b"mailbox/pair/pake/secret/v1";

/// The first message: session id and a blinded point.
pub const OFFER_BYTES: usize = pake_cpace::STEP1_PACKET_BYTES;
/// The reply: a blinded point.
pub const ANSWER_BYTES: usize = pake_cpace::STEP2_PACKET_BYTES;

/// A freshly generated pairing code, formatted for reading aloud.
///
/// Drawn from [`crate::Rng`] rather than the code being chosen by a person:
/// 45 bits is only 45 bits when every one of them is random, and a code
/// someone picked is worth a fraction of that.
pub fn generate_code(rng: &crate::Rng) -> String {
    let bytes = rng.bytes::<CODE_CHARS>();
    // `% 32` is unbiased here only because 256 is a whole multiple of 32:
    // every character is equally likely, and the 45 bits §8.1 costs its
    // security on are really 45. A 33-character alphabet would need rejection
    // sampling instead, and quietly would not get it.
    let raw: String = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % 32] as char)
        .collect();
    format(&raw)
}

/// Inserts the display hyphens.
fn format(raw: &str) -> String {
    raw.as_bytes()
        .chunks(GROUP)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Cleans up a code as a person typed it.
///
/// Hyphens and spaces are decoration and are dropped; case is irrelevant; and
/// Crockford's substitutions are applied, so someone who read `O` for `0` or
/// `I` for `1` gets the code they meant rather than a failed pairing they
/// cannot explain. Anything still outside the alphabet is an error here, where
/// it can be reported as a typo, rather than a handshake that mysteriously
/// yields the wrong secret.
pub fn normalize_code(input: &str) -> Result<String> {
    let mut out = String::with_capacity(CODE_CHARS);
    for c in input.chars() {
        if c == '-' || c == ' ' || c == '\t' || c == '_' {
            continue;
        }
        let upper = c.to_ascii_uppercase();
        let mapped = match upper {
            'I' | 'L' => '1',
            'O' => '0',
            other => other,
        };
        if !ALPHABET.contains(&(mapped as u8)) {
            return Err(Error::usage(format!(
                "`{c}` is not part of a pairing code; codes use digits and letters, \
                 without I, L, O, or U"
            )));
        }
        out.push(mapped);
        if out.len() > CODE_CHARS {
            return Err(Error::usage(format!(
                "a pairing code is {CODE_CHARS} characters; that one is longer"
            )));
        }
    }
    if out.len() != CODE_CHARS {
        return Err(Error::usage(format!(
            "a pairing code is {CODE_CHARS} characters; that one has {}",
            out.len()
        )));
    }
    Ok(out)
}

/// The 256-bit secret a completed handshake produces.
///
/// Deliberately the same shape as the secret the QR and LAN transports carry,
/// so everything downstream — sealing the payload, the short authentication
/// string, the freshness check — is the code that already exists and already
/// has tests. The PAKE replaces how the secret is *agreed*, and nothing else.
pub struct Established {
    secret: [u8; crypto::SECRET_BYTES],
}

impl Established {
    pub fn secret(&self) -> &[u8; crypto::SECRET_BYTES] {
        &self.secret
    }
}

// No `Debug` for the same reason `Payload` has none: this is a key.
impl std::fmt::Debug for Established {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Established(<secret>)")
    }
}

/// The initiator's state between sending the offer and receiving the answer.
pub struct Pending {
    inner: pake_cpace::Step1Out,
    offer: [u8; OFFER_BYTES],
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pending(<handshake>)")
    }
}

/// Starts a handshake. The initiator is whichever device speaks first on the
/// wire; the transport decides which that is.
pub fn begin(code: &str) -> Result<(Pending, [u8; OFFER_BYTES])> {
    let code = normalize_code(code)?;
    let inner = pake_cpace::CPace::step1(&code, ID_INITIATOR, ID_RESPONDER, Some(AD))
        .map_err(|_| Error::usage("the pairing handshake could not be started"))?;
    let offer = inner.packet();
    Ok((Pending { inner, offer }, offer))
}

impl Pending {
    /// The offer this device sent, for the transcript.
    pub fn offer(&self) -> &[u8; OFFER_BYTES] {
        &self.offer
    }

    /// Completes the handshake with the responder's answer.
    ///
    /// Succeeds whether or not the codes matched — see the module docs. What
    /// it cannot do is succeed on a malformed or degenerate point, which is
    /// the one thing the group check here is for.
    pub fn finish(&self, answer: &[u8; ANSWER_BYTES]) -> Result<Established> {
        let keys = self
            .inner
            .step3(answer)
            .map_err(|_| Error::usage("the other device sent an unusable handshake message"))?;
        Ok(Established {
            secret: mix(&keys.k1, &keys.k2),
        })
    }
}

/// The responder's half: takes the offer, answers it, and is done.
pub fn respond(code: &str, offer: &[u8; OFFER_BYTES]) -> Result<(Established, [u8; ANSWER_BYTES])> {
    let code = normalize_code(code)?;
    let out = pake_cpace::CPace::step2(offer, &code, ID_INITIATOR, ID_RESPONDER, Some(AD))
        .map_err(|_| Error::usage("the other device sent an unusable handshake message"))?;
    let keys = out.shared_keys();
    Ok((
        Established {
            secret: mix(&keys.k1, &keys.k2),
        },
        out.packet(),
    ))
}

/// Folds CPace's two directional keys into one secret.
///
/// CPace hands back a key per direction; §8 wants a single secret both sides
/// seal and verify under. Concatenating and running them through the same HKDF
/// the rest of pairing uses is the whole of it — both sides hold both keys, so
/// both compute the same answer.
fn mix(k1: &[u8; 32], k2: &[u8; 32]) -> [u8; crypto::SECRET_BYTES] {
    let mut ikm = [0u8; 64];
    ikm[..32].copy_from_slice(k1);
    ikm[32..].copy_from_slice(k2);
    crypto::derive::<{ crypto::SECRET_BYTES }>(&ikm, INFO_SECRET)
}

/// The transcript the short authentication string is computed over (§8.1).
///
/// Both handshake messages, in a fixed order and length-prefixed, so that two
/// devices that saw different bytes on the wire display different words —
/// which is the check that catches an active attacker sitting between them.
pub fn transcript(offer: &[u8; OFFER_BYTES], answer: &[u8; ANSWER_BYTES]) -> Vec<u8> {
    let mut out = Vec::with_capacity(OFFER_BYTES + ANSWER_BYTES + 2);
    out.push(OFFER_BYTES as u8);
    out.extend_from_slice(offer);
    out.push(ANSWER_BYTES as u8);
    out.extend_from_slice(answer);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_devices_with_the_same_code_agree_on_a_secret() {
        let code = "K7M-4QB-XZ2";
        let (pending, offer) = begin(code).expect("begin");
        let (responder, answer) = respond(code, &offer).expect("respond");
        let initiator = pending.finish(&answer).expect("finish");
        assert_eq!(initiator.secret(), responder.secret());
    }

    /// The secret is agreed, not transported: two runs of the same code give
    /// unrelated secrets, so a captured transcript is worth nothing later.
    #[test]
    fn the_same_code_twice_gives_different_secrets() {
        let code = "K7M-4QB-XZ2";
        let mut secrets = Vec::new();
        for _ in 0..2 {
            let (pending, offer) = begin(code).expect("begin");
            let (_, answer) = respond(code, &offer).expect("respond");
            secrets.push(*pending.finish(&answer).expect("finish").secret());
        }
        assert_ne!(secrets[0], secrets[1]);
    }

    /// The property the whole design rests on: a wrong code does not fail
    /// here. It succeeds, and yields a different secret — because an
    /// implementation that could report "wrong password" could be asked
    /// offline, which is exactly what a PAKE exists to prevent.
    #[test]
    fn a_wrong_code_succeeds_and_produces_a_different_secret() {
        let (pending, offer) = begin("K7M-4QB-XZ2").expect("begin");
        let (responder, answer) = respond("K7M-4QB-XZ3", &offer).expect("respond");
        let initiator = pending
            .finish(&answer)
            .expect("the handshake still completes");
        assert_ne!(
            initiator.secret(),
            responder.secret(),
            "mismatched codes must not agree"
        );
    }

    /// …and the mismatch is caught downstream, by the machinery that already
    /// exists: the payload does not authenticate, and the words differ.
    #[test]
    fn a_wrong_code_is_caught_by_the_seal_and_by_the_words() {
        let rng = crate::Rng::seeded(b"pake-mismatch");
        let (pending, offer) = begin("K7M-4QB-XZ2").expect("begin");
        let (responder, answer) = respond("K7M-4QB-XZ3", &offer).expect("respond");
        let initiator = pending.finish(&answer).expect("finish");

        let sealed = crypto::seal(initiator.secret(), b"v1", b"every credential", &rng);
        assert!(
            crypto::open(responder.secret(), b"v1", &sealed).is_err(),
            "a payload sealed under a mismatched secret must not open"
        );

        let script = transcript(&offer, &answer);
        assert_ne!(
            crypto::short_authentication_string(initiator.secret(), &script),
            crypto::short_authentication_string(responder.secret(), &script),
            "the words must differ, since they are what the user checks"
        );
    }

    /// Matching codes must of course produce matching words, or the check is
    /// noise the user learns to click through.
    #[test]
    fn matching_codes_show_the_same_words() {
        let code = "ABC-DEF-GHJ";
        let (pending, offer) = begin(code).expect("begin");
        let (responder, answer) = respond(code, &offer).expect("respond");
        let initiator = pending.finish(&answer).expect("finish");

        let script = transcript(&offer, &answer);
        assert_eq!(
            crypto::short_authentication_string(initiator.secret(), &script),
            crypto::short_authentication_string(responder.secret(), &script),
        );
    }

    /// An active attacker who changed a byte in flight changes the transcript,
    /// and the words are what reveals it.
    #[test]
    fn a_tampered_transcript_changes_the_words() {
        let code = "ABC-DEF-GHJ";
        let (pending, offer) = begin(code).expect("begin");
        let (_, answer) = respond(code, &offer).expect("respond");
        let established = pending.finish(&answer).expect("finish");

        let honest = transcript(&offer, &answer);
        let mut tampered_answer = answer;
        tampered_answer[0] ^= 0x01;
        let tampered = transcript(&offer, &tampered_answer);

        assert_ne!(
            crypto::short_authentication_string(established.secret(), &honest),
            crypto::short_authentication_string(established.secret(), &tampered),
        );
    }

    #[test]
    fn a_degenerate_answer_is_refused() {
        let (pending, offer) = begin("ABC-DEF-GHJ").expect("begin");
        let _ = offer;
        // The identity element: a classic way to force a known shared point.
        assert!(pending.finish(&[0u8; ANSWER_BYTES]).is_err());
    }

    // ---- the code itself ----

    #[test]
    fn a_generated_code_is_nine_characters_and_reads_back() {
        let rng = crate::Rng::seeded(b"codes");
        for _ in 0..64 {
            let code = generate_code(&rng);
            assert_eq!(code.len(), CODE_CHARS + 2, "XXX-XXX-XXX: {code}");
            let normalized = normalize_code(&code).expect("its own code");
            assert_eq!(normalized.len(), CODE_CHARS);
            assert!(normalized.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        }
    }

    /// Crockford's whole point: the glyphs people confuse are accepted as what
    /// they meant. Someone reading `0` off a screen and typing `O` should pair,
    /// not be told their code is wrong.
    #[test]
    fn confusable_glyphs_are_read_as_what_was_meant() {
        assert_eq!(normalize_code("O12-345-678").expect("O"), "012345678");
        assert_eq!(normalize_code("I12-345-678").expect("I"), "112345678");
        assert_eq!(normalize_code("L12-345-678").expect("L"), "112345678");
        // …and case and decoration do not matter either.
        assert_eq!(normalize_code("k7m 4qb xz2").expect("spaces"), "K7M4QBXZ2");
        assert_eq!(normalize_code("k7m4qbxz2").expect("bare"), "K7M4QBXZ2");
    }

    #[test]
    fn a_code_that_is_not_one_is_a_usage_error_not_a_failed_pairing() {
        assert!(normalize_code("K7M-4QB").is_err(), "too short");
        assert!(normalize_code("K7M-4QB-XZ22").is_err(), "too long");
        assert!(
            normalize_code("K7M-4QB-XZ!").is_err(),
            "not in the alphabet"
        );
        // `U` is deliberately not in Crockford's alphabet.
        assert!(normalize_code("K7M-4QB-XZU").is_err(), "U is excluded");
    }

    /// The alphabet is a wire format: a code generated by one build has to be
    /// typeable into another, so changing this silently breaks pairing.
    #[test]
    fn the_alphabet_is_pinned() {
        assert_eq!(ALPHABET, b"0123456789ABCDEFGHJKMNPQRSTVWXYZ");
        assert_eq!(ALPHABET.len(), 32, "five bits a character");
        for excluded in *b"ILOU" {
            assert!(!ALPHABET.contains(&excluded));
        }
    }

    /// §8.1 costs its security on 45 bits. If the code ever gets shorter, that
    /// number has to be re-argued rather than quietly reduced.
    #[test]
    fn the_code_carries_the_entropy_the_spec_costed() {
        assert_eq!(CODE_CHARS * 5, 45);
    }

    /// The derivation label is a wire format like every other one in `pair`.
    #[test]
    fn the_derivation_label_is_pinned() {
        assert_eq!(INFO_SECRET, b"mailbox/pair/pake/secret/v1");
        assert_eq!(AD, b"mailbox/pair/pake/v1");
        assert_eq!(ID_INITIATOR, "mailbox/pair/initiator/v1");
        assert_eq!(ID_RESPONDER, "mailbox/pair/responder/v1");
        // Fixed inputs, fixed output: the fold is not accidentally random.
        assert_eq!(
            crate::hex::encode(&mix(&[1u8; 32], &[2u8; 32])),
            crate::hex::encode(&mix(&[1u8; 32], &[2u8; 32]))
        );
        assert_ne!(
            crate::hex::encode(&mix(&[1u8; 32], &[2u8; 32])),
            crate::hex::encode(&mix(&[2u8; 32], &[1u8; 32])),
            "the two directional keys are not interchangeable"
        );
    }

    #[test]
    fn the_transcript_is_unambiguous() {
        // Length-prefixed, so no two different (offer, answer) pairs can
        // produce the same bytes by sliding the boundary.
        let a = transcript(&[0xAA; OFFER_BYTES], &[0xBB; ANSWER_BYTES]);
        let b = transcript(&[0xAB; OFFER_BYTES], &[0xBB; ANSWER_BYTES]);
        assert_ne!(a, b);
        assert_eq!(a.len(), OFFER_BYTES + ANSWER_BYTES + 2);
    }

    /// A secret must not be printable by accident. `Payload` learned this the
    /// hard way and these types get the same treatment.
    #[test]
    fn the_types_do_not_print_their_secrets() {
        let code = "ABC-DEF-GHJ";
        let (pending, offer) = begin(code).expect("begin");
        let (established, answer) = respond(code, &offer).expect("respond");
        let _ = answer;
        assert!(!format!("{pending:?}").contains("secret"));
        assert_eq!(format!("{established:?}"), "Established(<secret>)");
        assert!(!format!("{established:?}").contains(&crate::hex::encode(established.secret())));
    }
}

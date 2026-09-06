//! L0: known-answer vectors for the pairing cryptography (§8.1, §15).
//!
//! > **L0** known-answer vectors for the AEAD and the payload codec […]
//! > No new level, but pairing does not ship without these.
//!
//! Two kinds of vector here, and the distinction matters.
//!
//! The first are *external*: RFC 5869's HKDF-SHA256 answers, checking that the
//! primitive underneath does what its specification says. Those catch a wrong
//! crate, a wrong hash, a misused salt.
//!
//! The second are *ours*: fixed outputs for a fixed secret, pinning the info
//! strings and the derivation shape. Those catch the failure that has no error
//! message — a label changed by one character, after which two versions of
//! mailbox silently cannot pair and neither can say why.

use mailbox_core::clock::{Clock, Rng};
use mailbox_core::pair::crypto::{self, FRESHNESS_SECONDS};
use mailbox_core::pair::{PairedAccount, PairedCredential, PairingUri, Payload, PAYLOAD_VERSION};

fn hex(bytes: &[u8]) -> String {
    mailbox_core::hex::encode(bytes)
}

fn unhex(text: &str) -> Vec<u8> {
    mailbox_core::hex::decode(text).expect("hex")
}

/// A fixed secret for every vector below. Not random, on purpose: a vector
/// generated at test time is not a vector.
fn secret() -> Vec<u8> {
    unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
}

// ---- external vectors: the primitive is what it claims to be ----

/// RFC 5869 test case 1 for HKDF-SHA256.
///
/// Our `derive` uses no salt, which RFC 5869 defines as a string of
/// `HashLen` zeros — so the vector is reproduced with that salt to check the
/// expand step and the output length handling.
#[test]
fn hkdf_sha256_matches_rfc5869() {
    use hkdf::Hkdf;
    use sha2::Sha256;

    let ikm = unhex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let salt = unhex("000102030405060708090a0b0c");
    let info = unhex("f0f1f2f3f4f5f6f7f8f9");

    let hkdf = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut okm = [0u8; 42];
    hkdf.expand(&info, &mut okm).expect("expand");

    assert_eq!(
        hex(&okm),
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
    );
}

/// RFC 5869 test case 3: no salt, no info — the shape our own derivation uses.
#[test]
fn hkdf_sha256_with_no_salt_matches_rfc5869() {
    let ikm = unhex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
    let okm: [u8; 42] = crypto::derive(&ikm, b"");
    assert_eq!(
        hex(&okm),
        "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
    );
}

/// XChaCha20-Poly1305 round-trips, and rejects every tampering.
#[test]
fn the_aead_authenticates_what_it_should() {
    let rng = Rng::seeded(b"aead");
    let aad = Payload::aad(PAYLOAD_VERSION);
    let sealed = crypto::seal(&secret(), &aad, b"the payload", &rng);

    assert_eq!(
        crypto::open(&secret(), &aad, &sealed).expect("opens"),
        b"the payload"
    );

    // A different secret.
    let mut other = secret();
    other[0] ^= 1;
    assert!(crypto::open(&other, &aad, &sealed).is_err());

    // A different schema version — the AAD is what makes the version
    // authenticated even though it travels in the clear (§8.1).
    assert!(crypto::open(&secret(), &Payload::aad(PAYLOAD_VERSION + 1), &sealed).is_err());

    // One flipped bit, anywhere.
    for i in 0..sealed.len() {
        let mut tampered = sealed.clone();
        tampered[i] ^= 0x01;
        assert!(
            crypto::open(&secret(), &aad, &tampered).is_err(),
            "a flipped bit at {i} was accepted"
        );
    }

    // Truncation.
    for cut in [0, 1, crypto::NONCE_BYTES, sealed.len() - 1] {
        assert!(crypto::open(&secret(), &aad, &sealed[..cut]).is_err());
    }
}

/// Every failure reads the same. Distinguishing a wrong key from a tampered
/// byte would hand an attacker an oracle.
#[test]
fn every_aead_failure_looks_alike() {
    let rng = Rng::seeded(b"aead");
    let aad = Payload::aad(PAYLOAD_VERSION);
    let sealed = crypto::seal(&secret(), &aad, b"x", &rng);

    let mut wrong_key = secret();
    wrong_key[31] ^= 0xff;
    let mut tampered = sealed.clone();
    tampered[crypto::NONCE_BYTES] ^= 0xff;

    let messages = [
        crypto::open(&wrong_key, &aad, &sealed)
            .unwrap_err()
            .to_string(),
        crypto::open(&secret(), &aad, &tampered)
            .unwrap_err()
            .to_string(),
        crypto::open(&secret(), &aad, &sealed[..4])
            .unwrap_err()
            .to_string(),
        crypto::open(&secret(), b"other", &sealed)
            .unwrap_err()
            .to_string(),
    ];
    assert!(
        messages.windows(2).all(|w| w[0] == w[1]),
        "the failures are distinguishable: {messages:?}"
    );
}

/// A fresh nonce per sealing, so two seals of the same plaintext differ.
#[test]
fn sealing_is_randomised() {
    let rng = Rng::from_env();
    let aad = Payload::aad(PAYLOAD_VERSION);
    let a = crypto::seal(&secret(), &aad, b"same", &rng);
    let b = crypto::seal(&secret(), &aad, b"same", &rng);
    assert_ne!(a, b, "XChaCha20-Poly1305 needs a fresh nonce per message");
    assert_eq!(crypto::open(&secret(), &aad, &a).unwrap(), b"same");
    assert_eq!(crypto::open(&secret(), &aad, &b).unwrap(), b"same");
}

// ---- our vectors: the labels are pinned ----

/// The channel id and key for a fixed secret.
///
/// Unlike the RFC vectors above, these have no external authority — they were
/// recorded from this implementation. That is exactly why they need pinning:
/// changing `INFO_CHANNEL` by one character makes two builds unable to pair,
/// and nothing anywhere would say so. `print_the_vectors` re-records them, and
/// doing that should be a deliberate act with a reason in the commit message.
#[test]
fn the_derivation_labels_are_pinned() {
    assert_eq!(
        crypto::channel_id(&secret()),
        "1a032a8040d5950453e2f2c1758a24e5",
        "INFO_CHANNEL changed — this is a silent pairing incompatibility"
    );
    assert_eq!(
        hex(&crypto::payload_key(&secret())),
        "7a96c73edc539a3727312badc6324c6f8e10bcdc2072dc01ef11da7b2c423d18",
        "INFO_KEY changed — this is a silent pairing incompatibility"
    );
}

/// The SAS for a fixed secret and transcript (§8.3's `amber-tiger-04-mint`).
#[test]
fn the_short_authentication_string_is_pinned() {
    let transcript = crypto::transcript("channel", b"sealed bytes");
    let sas = crypto::short_authentication_string(&secret(), &transcript);

    // Shape first: four hyphen-separated tokens, one of them two digits.
    let parts: Vec<&str> = sas.split('-').collect();
    assert_eq!(parts.len(), 4, "{sas}");
    assert!(
        parts[2].len() == 2 && parts[2].chars().all(|c| c.is_ascii_digit()),
        "{sas}"
    );
    assert!(
        parts.iter().all(|p| !p.is_empty()),
        "{sas} has an empty token"
    );

    assert_eq!(sas, "lemon-canyon-80-comet", "the SAS derivation changed");
}

/// The whole point of the SAS: it says both sides saw the *same conversation*,
/// not merely that they know the secret. A substituted payload changes it.
#[test]
fn the_sas_changes_with_the_transcript() {
    let base =
        crypto::short_authentication_string(&secret(), &crypto::transcript("channel", b"sealed"));

    assert_ne!(
        base,
        crypto::short_authentication_string(&secret(), &crypto::transcript("channel", b"sealed!")),
        "a changed payload must change the words"
    );
    assert_ne!(
        base,
        crypto::short_authentication_string(&secret(), &crypto::transcript("other", b"sealed")),
        "a changed channel must change the words"
    );

    let mut other = secret();
    other[0] ^= 1;
    assert_ne!(
        base,
        crypto::short_authentication_string(&other, &crypto::transcript("channel", b"sealed")),
        "a different secret must change the words"
    );
}

/// A transcript cannot be forged by moving a byte from one field to the next.
#[test]
fn the_transcript_is_unambiguous() {
    assert_ne!(
        crypto::transcript("ab", b"cd"),
        crypto::transcript("abc", b"d"),
        "length prefixes are what stop two conversations colliding"
    );
}

// ---- the payload codec ----

fn sample_payload(clock: &Clock, rng: &Rng) -> Payload {
    Payload {
        version: PAYLOAD_VERSION,
        created: clock.now(),
        nonce: rng.hex(16),
        device: "laptop".into(),
        accounts: vec![PairedAccount {
            id: "work".into(),
            config: mailbox_core::config::AccountConfig {
                address: "peter@example.com".into(),
                imap_host: Some("imap.example.com".into()),
                ..Default::default()
            },
            credential: Some(PairedCredential::Password {
                secret: "hunter2".into(),
            }),
        }],
        preferences: None,
        state_key: None,
    }
}

#[test]
fn a_payload_round_trips_through_the_seal() {
    let clock = Clock::fixed(1_788_264_000);
    let rng = Rng::seeded(b"payload");
    let payload = sample_payload(&clock, &rng);

    let sealed = payload.seal(&secret(), &rng).expect("seal");
    let opened = Payload::open(&secret(), &sealed, clock.now()).expect("open");

    assert_eq!(opened.device, "laptop");
    assert_eq!(opened.accounts.len(), 1);
    assert_eq!(opened.accounts[0].id, "work");
    match &opened.accounts[0].credential {
        Some(PairedCredential::Password { secret }) => assert_eq!(secret, "hunter2"),
        other => panic!("the credential did not survive: {}", other.is_some()),
    }
}

/// §8.1: a receiver rejects a payload older than the TTL.
#[test]
fn a_stale_payload_is_refused() {
    let clock = Clock::fixed(1_788_264_000);
    let rng = Rng::seeded(b"payload");
    let sealed = sample_payload(&clock, &rng).seal(&secret(), &rng).unwrap();

    // Inside the window.
    assert!(Payload::open(&secret(), &sealed, clock.now() + FRESHNESS_SECONDS - 1).is_ok());

    // Past it.
    let err = Payload::open(&secret(), &sealed, clock.now() + FRESHNESS_SECONDS + 1)
        .expect_err("a stale payload must be refused");
    assert!(err.to_string().contains("seconds old"), "{err}");

    // And from the future: a clock problem or a replay, and accepting it would
    // defeat the window entirely.
    assert!(Payload::open(&secret(), &sealed, clock.now() - FRESHNESS_SECONDS - 1).is_err());
}

/// A payload never carries the mail store (§8) — only definitions and secrets.
#[test]
fn the_payload_carries_no_mail() {
    let clock = Clock::fixed(1_788_264_000);
    let rng = Rng::seeded(b"payload");
    let payload = sample_payload(&clock, &rng);
    let json = serde_json::to_string(&payload).unwrap();

    for forbidden in ["blob", "message", "meta.db", "folder", "thread"] {
        assert!(
            !json.contains(forbidden),
            "a pairing payload should not mention {forbidden}: {json}"
        );
    }
}

// ---- the pairing link ----

#[test]
fn a_pairing_link_round_trips() {
    let uri = PairingUri {
        secret: secret().try_into().unwrap(),
        address: Some("192.168.1.20:41234".into()),
    };
    let text = uri.render();
    assert!(text.starts_with("mailbox://pair/"), "{text}");
    // §8.1 puts the secret in the fragment.
    assert!(text.contains('#'), "{text}");
    assert_eq!(PairingUri::parse(&text).expect("parse"), uri);

    let bare = PairingUri {
        secret: secret().try_into().unwrap(),
        address: None,
    };
    assert_eq!(PairingUri::parse(&bare.render()).expect("parse"), bare);
}

#[test]
fn a_malformed_link_is_refused() {
    for bad in [
        "",
        "https://example.com",
        "mailbox://pair",
        "mailbox://pair#",
        "mailbox://pair#nothex",
        // A secret of the wrong length is the dangerous one: silently padding
        // it would make a weak secret look like a strong one.
        "mailbox://pair#0011",
    ] {
        assert!(PairingUri::parse(bad).is_err(), "{bad:?} should be refused");
    }
}

/// Prints the current values, for recording a new baseline deliberately.
///
/// Ignored: it exists so that changing a label is a two-step act — see the new
/// values, then paste them in — rather than something a `--bless` flag does
/// while nobody is looking.
#[test]
#[ignore = "prints the vectors; run when deliberately re-baselining"]
fn print_the_vectors() {
    println!("channel_id  = {:?}", crypto::channel_id(&secret()));
    println!("payload_key = {:?}", hex(&crypto::payload_key(&secret())));
    println!(
        "sas         = {:?}",
        crypto::short_authentication_string(
            &secret(),
            &crypto::transcript("channel", b"sealed bytes")
        )
    );
}

/// §8: "No re-entering passwords, no re-doing OAuth."
///
/// The second half of that is only true if what travels is the *refresh*
/// token. An access token expires about an hour after the pairing, and a
/// device that had to log in again would make the promise false in the least
/// visible way — it would work, on the day it was set up.
#[test]
fn an_oauth_account_pairs_its_refresh_token_and_not_an_access_token() {
    let dir = std::env::temp_dir().join(format!("mailbox-pair-oauth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("oauthy")).expect("temp dir");
    // The token cache lives under the state directory, so point that here.
    mailbox_core::paths::set_state_root(&dir);

    mailbox_core::oauth::cache(
        "oauthy",
        &mailbox_core::oauth::Tokens {
            access_token: "an-access-token".into(),
            refresh_token: Some("the-refresh-token".into()),
            expires_at: i64::MAX,
        },
    )
    .expect("cache the tokens");

    let mut config = mailbox_core::config::Config::default();
    config.accounts.insert(
        "oauthy".into(),
        mailbox_core::config::AccountConfig {
            address: "peter@gmail.com".into(),
            imap_host: Some("imap.gmail.com".into()),
            oauth_provider: Some("google".into()),
            oauth_client_id: Some("abc".into()),
            ..Default::default()
        },
    );

    let clock = Clock::fixed(1_788_264_000);
    let rng = Rng::seeded(b"pair-oauth");
    let payload = Payload::build(&config, "laptop", &clock, &rng);

    match &payload.accounts[0].credential {
        Some(PairedCredential::OAuth2 { refresh_token }) => {
            assert_eq!(
                refresh_token, "the-refresh-token",
                "the access token would have expired within the hour"
            );
        }
        other => panic!("expected an OAuth2 credential, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

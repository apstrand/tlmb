//! L0: building outgoing messages (§7, §6.1) and the send policy (§6.5).
//!
//! Both are pure functions, so both test as tables. The most important cases
//! here are not formatting: they are header injection, which is a security
//! property, and `Bcc` staying out of the bytes, which is the whole of what
//! makes a blind copy blind.

use mailbox_api::Address;
use mailbox_core::clock::{Clock, Rng};
use mailbox_core::compose::{self, Attachment, Draft};
use mailbox_core::policy::{Policy, SendContext};

fn clock() -> Clock {
    Clock::fixed(1_788_264_000) // 2026-09-01T12:00:00Z
}

fn rng() -> Rng {
    Rng::seeded(b"compose-test")
}

fn addr(s: &str) -> Address {
    Address::new(None, s)
}

fn named(name: &str, s: &str) -> Address {
    Address::new(Some(name.to_string()), s)
}

fn basic() -> Draft {
    Draft {
        from: Some(addr("peter@example.com")),
        to: vec![addr("alice@example.com")],
        subject: "Hello".into(),
        body: "Hi there.".into(),
        ..Default::default()
    }
}

fn build(draft: &Draft) -> String {
    let bytes = compose::build(draft, &clock(), &rng(), "example.com").expect("build");
    String::from_utf8(bytes).expect("utf-8")
}

#[test]
fn a_plain_message_has_the_headers_it_should() {
    let out = build(&basic());
    assert!(out.contains("From: peter@example.com\r\n"), "{out}");
    assert!(out.contains("To: alice@example.com\r\n"), "{out}");
    assert!(out.contains("Subject: Hello\r\n"), "{out}");
    assert!(out.contains("Date: "), "{out}");
    assert!(out.contains("Message-ID: <"), "{out}");
    assert!(out.contains("MIME-Version: 1.0\r\n"), "{out}");
    assert!(out.ends_with("Hi there."), "{out}");
    // CRLF throughout: a bare LF in a message is a protocol violation that
    // some servers reject and others silently rewrite.
    assert!(
        !out.replace("\r\n", "").contains('\n'),
        "bare LF in the output"
    );
}

/// §6.7: the same draft, clock, and seed produce the same bytes. This is what
/// lets `--dry-run` show *the* bytes rather than bytes like them.
#[test]
fn building_is_deterministic() {
    assert_eq!(build(&basic()), build(&basic()));
}

#[test]
fn a_different_seed_changes_the_message_id() {
    let draft = basic();
    let a = compose::build(&draft, &clock(), &Rng::seeded(b"one"), "example.com").unwrap();
    let b = compose::build(&draft, &clock(), &Rng::seeded(b"two"), "example.com").unwrap();
    assert_ne!(
        a, b,
        "the Message-ID must not be predictable across accounts"
    );
}

/// The `Message-ID` domain comes from the sender, not the host: one naming the
/// laptop it was written on tells every recipient something they did not need.
#[test]
fn the_message_id_uses_the_senders_domain() {
    assert_eq!(compose::domain_for("peter@Example.COM"), "example.com");
    assert_eq!(compose::domain_for("nonsense"), "localhost");

    let out = build(&basic());
    let mid = out
        .lines()
        .find_map(|l| l.strip_prefix("Message-ID: "))
        .expect("a Message-ID");
    assert!(mid.ends_with("@example.com>"), "{mid}");
}

/// The security property: a header value containing a newline is not a long
/// header, it is a second header.
#[test]
fn header_injection_is_refused() {
    let attempts = [
        "Innocent\r\nBcc: attacker@evil.example",
        "Innocent\nBcc: attacker@evil.example",
        "Innocent\rX-Injected: yes",
        "Innocent\0truncated",
    ];
    for subject in attempts {
        let draft = Draft {
            subject: subject.into(),
            ..basic()
        };
        let err = compose::build(&draft, &clock(), &rng(), "example.com")
            .expect_err(&format!("{subject:?} should be refused"));
        assert!(err.to_string().contains("newline"), "{err}");
    }

    // And through a display name, which is the less obvious way in.
    let draft = Draft {
        to: vec![named(
            "Alice\r\nBcc: attacker@evil.example",
            "alice@example.com",
        )],
        ..basic()
    };
    assert!(compose::build(&draft, &clock(), &rng(), "example.com").is_err());
}

/// Rejected rather than stripped: a subject that silently loses half of itself
/// is a bug the user finds later.
#[test]
fn injection_is_refused_not_sanitized() {
    let draft = Draft {
        subject: "Before\r\nAfter".into(),
        ..basic()
    };
    assert!(compose::build(&draft, &clock(), &rng(), "example.com").is_err());
}

/// §7: Bcc reaches the envelope and no header.
#[test]
fn bcc_never_appears_in_the_bytes() {
    let draft = Draft {
        bcc: vec![addr("hidden@example.com")],
        ..basic()
    };
    let out = build(&draft);
    assert!(!out.contains("hidden@example.com"), "{out}");
    assert!(!out.to_lowercase().contains("bcc"), "{out}");

    // But it is on the envelope, which is what actually delivers it.
    assert!(draft
        .envelope_recipients()
        .contains(&"hidden@example.com".to_string()));
}

#[test]
fn the_envelope_covers_to_cc_and_bcc_without_duplicates() {
    let draft = Draft {
        to: vec![addr("a@example.com"), addr("A@Example.com")],
        cc: vec![addr("b@example.com")],
        bcc: vec![addr("c@example.com"), addr("a@example.com")],
        ..basic()
    };
    let mut r = draft.envelope_recipients();
    r.sort();
    assert_eq!(r, vec!["a@example.com", "b@example.com", "c@example.com"]);
}

#[test]
fn non_ascii_headers_are_encoded() {
    let draft = Draft {
        subject: "Hej på dig".into(),
        to: vec![named("Ärendehantering", "arende@example.com")],
        ..basic()
    };
    let out = build(&draft);
    assert!(out.contains("Subject: =?UTF-8?B?"), "{out}");
    assert!(out.contains("=?UTF-8?B?"), "{out}");
    // The header block must stay ASCII, which is the point of encoding it.
    let head = out.split("\r\n\r\n").next().unwrap();
    assert!(head.is_ascii(), "the header block must be ASCII: {head}");
}

#[test]
fn a_non_ascii_body_is_quoted_printable() {
    let draft = Draft {
        body: "Hej på dig".into(),
        ..basic()
    };
    let out = build(&draft);
    assert!(
        out.contains("Content-Transfer-Encoding: quoted-printable"),
        "{out}"
    );
    assert!(out.contains("=C3=A5"), "å should be encoded: {out}");
}

#[test]
fn an_ascii_body_stays_readable() {
    let out = build(&basic());
    assert!(out.contains("Content-Transfer-Encoding: 7bit"), "{out}");
    assert!(out.contains("Hi there."), "{out}");
}

#[test]
fn quoted_printable_encodes_what_it_must_and_no_more() {
    assert_eq!(compose::quoted_printable("plain"), "plain");
    assert_eq!(compose::quoted_printable("a=b"), "a=3Db");
    // Trailing whitespace is encoded: a gateway that strips it would change
    // the body's bytes.
    assert_eq!(compose::quoted_printable("a \r\n"), "a=20\r\n");
    assert!(compose::quoted_printable("å").contains("=C3=A5"));
    // No output line exceeds the 76-character limit.
    let long = "x".repeat(500);
    for line in compose::quoted_printable(&long).split("\r\n") {
        assert!(line.len() <= 76, "line too long: {}", line.len());
    }
}

/// The built bytes must parse back to what the draft said. Same round-trip
/// trick the fixture format uses, and it catches a whole class of framing bugs
/// at once.
#[test]
fn a_built_message_parses_back() {
    let draft = Draft {
        cc: vec![addr("carol@example.com")],
        subject: "Round trip".into(),
        body: "The body.".into(),
        in_reply_to: Some("<parent@example.com>".into()),
        references: vec!["<root@example.com>".into(), "<parent@example.com>".into()],
        ..basic()
    };
    let bytes = compose::build(&draft, &clock(), &rng(), "example.com").unwrap();
    let parsed = mailbox_core::mime::parse(&bytes);

    assert_eq!(parsed.subject, "Round trip");
    assert_eq!(parsed.from[0].address, "peter@example.com");
    assert_eq!(parsed.to[0].address, "alice@example.com");
    assert_eq!(parsed.cc[0].address, "carol@example.com");
    assert_eq!(parsed.text.as_deref().map(str::trim), Some("The body."));
    assert_eq!(parsed.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(parsed.references.len(), 2);
    assert_eq!(parsed.date, Some(1_788_264_000));
}

#[test]
fn attachments_round_trip() {
    let draft = Draft {
        attachments: vec![Attachment {
            filename: "notes.txt".into(),
            mime_type: "text/plain".into(),
            content: b"attached bytes".to_vec(),
            inline: false,
            content_id: None,
        }],
        ..basic()
    };
    let bytes = compose::build(&draft, &clock(), &rng(), "example.com").unwrap();
    let parsed = mailbox_core::mime::parse(&bytes);

    assert!(parsed.has_attachment);
    let att: Vec<_> = parsed
        .attachments
        .iter()
        .filter(|a| a.is_attachment)
        .collect();
    assert_eq!(att.len(), 1);
    assert_eq!(att[0].filename.as_deref(), Some("notes.txt"));
    // The body survived alongside it.
    assert_eq!(parsed.text.as_deref().map(str::trim), Some("Hi there."));
}

#[test]
fn html_becomes_a_multipart_alternative() {
    let draft = Draft {
        html: Some("<p>Hi <b>there</b>.</p>".into()),
        ..basic()
    };
    let bytes = compose::build(&draft, &clock(), &rng(), "example.com").unwrap();
    let parsed = mailbox_core::mime::parse(&bytes);
    assert_eq!(parsed.text.as_deref().map(str::trim), Some("Hi there."));
    assert!(parsed
        .html
        .as_deref()
        .unwrap_or("")
        .contains("<b>there</b>"));
}

#[test]
fn a_message_needs_a_sender_and_a_recipient() {
    let no_from = Draft {
        from: None,
        ..basic()
    };
    assert!(compose::build(&no_from, &clock(), &rng(), "example.com").is_err());

    let no_to = Draft {
        to: vec![],
        ..basic()
    };
    assert!(compose::build(&no_to, &clock(), &rng(), "example.com").is_err());
}

#[test]
fn a_malformed_address_is_refused() {
    let draft = Draft {
        to: vec![addr("not-an-address")],
        ..basic()
    };
    assert!(compose::build(&draft, &clock(), &rng(), "example.com").is_err());
}

// ---- §6.5: the send policy ----

fn policy(toml: &str) -> Policy {
    toml::from_str(toml).expect("policy parses")
}

fn draft_to(recipients: &[&str]) -> Draft {
    Draft {
        to: recipients.iter().map(|r| addr(r)).collect(),
        ..basic()
    }
}

#[test]
fn an_empty_policy_allows_everything() {
    let p = Policy::default();
    assert!(p
        .check(
            &draft_to(&["anyone@anywhere.example"]),
            &SendContext::default()
        )
        .is_ok());
}

#[test]
fn a_denied_recipient_names_the_rule() {
    let p = policy("[send]\ndeny_recipients = [\"*@blocked.example\"]\n");
    let v = p
        .check(
            &draft_to(&["someone@blocked.example"]),
            &SendContext::default(),
        )
        .expect_err("should be denied");
    assert_eq!(v.rule, "send.deny_recipients");
    assert!(v.detail.contains("blocked.example"), "{}", v.detail);

    assert!(p
        .check(
            &draft_to(&["someone@allowed.example"]),
            &SendContext::default()
        )
        .is_ok());
}

/// An allowlist is a default-deny: anything matching nothing on it is refused.
#[test]
fn an_allowlist_denies_by_default() {
    let p = policy("[send]\nallow_recipients = [\"*@example.com\"]\n");
    assert!(p
        .check(&draft_to(&["alice@example.com"]), &SendContext::default())
        .is_ok());
    let v = p
        .check(
            &draft_to(&["alice@elsewhere.example"]),
            &SendContext::default(),
        )
        .expect_err("should be denied");
    assert_eq!(v.rule, "send.allow_recipients");
}

/// A denial is checked after the allowlist, so a narrow rule can carve a hole
/// in a broad permission.
#[test]
fn a_denial_overrides_an_allowance() {
    let p = policy(
        "[send]\nallow_recipients = [\"*@example.com\"]\ndeny_recipients = [\"ceo@example.com\"]\n",
    );
    assert!(p
        .check(&draft_to(&["alice@example.com"]), &SendContext::default())
        .is_ok());
    assert_eq!(
        p.check(&draft_to(&["ceo@example.com"]), &SendContext::default())
            .unwrap_err()
            .rule,
        "send.deny_recipients"
    );
}

#[test]
fn the_recipient_cap_counts_the_whole_envelope() {
    let p = policy("[send]\nmax_recipients = 2\n");
    let draft = Draft {
        to: vec![addr("a@example.com")],
        cc: vec![addr("b@example.com")],
        bcc: vec![addr("c@example.com")],
        ..basic()
    };
    assert_eq!(
        p.check(&draft, &SendContext::default()).unwrap_err().rule,
        "send.max_recipients"
    );
}

#[test]
fn the_hourly_rate_limit_uses_the_callers_count() {
    let p = policy("[send]\nmax_per_hour = 5\n");
    let under = SendContext {
        sends_last_hour: 4,
        ..Default::default()
    };
    assert!(p.check(&draft_to(&["a@example.com"]), &under).is_ok());

    let at_limit = SendContext {
        sends_last_hour: 5,
        ..Default::default()
    };
    assert_eq!(
        p.check(&draft_to(&["a@example.com"]), &at_limit)
            .unwrap_err()
            .rule,
        "send.max_per_hour"
    );
}

#[test]
fn external_recipients_can_require_confirmation() {
    let p = policy("[send]\nconfirm_external = true\ninternal_domains = [\"example.com\"]\n");
    let ctx = SendContext::default();

    assert!(p.check(&draft_to(&["alice@example.com"]), &ctx).is_ok());
    // A subdomain of an internal domain is internal.
    assert!(p.check(&draft_to(&["bob@mail.example.com"]), &ctx).is_ok());

    let v = p
        .check(&draft_to(&["stranger@elsewhere.example"]), &ctx)
        .unwrap_err();
    assert_eq!(v.rule, "send.confirm_external");

    // `--yes` satisfies it.
    let confirmed = SendContext {
        confirmed: true,
        ..Default::default()
    };
    assert!(p
        .check(&draft_to(&["stranger@elsewhere.example"]), &confirmed)
        .is_ok());
}

#[test]
fn the_attachment_cap_counts_bytes() {
    let p = policy("[send]\nmax_attachment_bytes = 100\n");
    let draft = Draft {
        attachments: vec![Attachment {
            filename: "big.bin".into(),
            mime_type: "application/octet-stream".into(),
            content: vec![0u8; 200],
            inline: false,
            content_id: None,
        }],
        ..basic()
    };
    assert_eq!(
        p.check(&draft, &SendContext::default()).unwrap_err().rule,
        "send.max_attachment_bytes"
    );
}

#[test]
fn recipient_patterns_are_globs_and_case_insensitive() {
    use mailbox_core::policy::matches;

    assert!(matches("*@example.com", "alice@example.com"));
    assert!(matches("*@example.com", "ALICE@EXAMPLE.COM"));
    assert!(matches("alice@*", "alice@anywhere.example"));
    assert!(matches("*", "anything@at.all"));
    assert!(matches("a?ice@example.com", "alice@example.com"));

    assert!(!matches("*@example.com", "alice@example.org"));
    assert!(!matches("alice@example.com", "bob@example.com"));
    // A pattern is not a substring match.
    assert!(!matches("example.com", "alice@example.com"));
}

/// A pathological pattern must not hang. `*` backtracking is where a naive
/// matcher goes exponential.
#[test]
fn glob_matching_does_not_blow_up() {
    use mailbox_core::policy::matches;
    let pattern = "*a*a*a*a*a*a*a*a*a*b";
    let subject = "a".repeat(64);
    assert!(!matches(pattern, &subject));
}

/// A reply to a reply is still `Re:` once. Taking the first suffix that
/// normalizes the same way returns the whole subject — it always does, that
/// being what normalizing means — so the prefix survived and compounded down
/// the thread.
#[test]
fn a_reply_prefix_is_not_doubled() {
    use mailbox_core::send::reply_subject_for_test as subject;
    use mailbox_core::send::ReplyMode;

    assert_eq!(subject("the parser", ReplyMode::Sender), "Re: the parser");
    assert_eq!(
        subject("Re: the parser", ReplyMode::Sender),
        "Re: the parser"
    );
    assert_eq!(
        subject("RE: Re: the parser", ReplyMode::Sender),
        "Re: the parser"
    );
    // Other languages' prefixes are stripped too (§5.6's normalizer), and the
    // reply goes out in ours.
    assert_eq!(
        subject("AW: the parser", ReplyMode::Sender),
        "Re: the parser"
    );
    // Casing of what remains is the sender's, not folded.
    assert_eq!(
        subject("Re: The Quarterly Report", ReplyMode::Sender),
        "Re: The Quarterly Report"
    );
    // A forward of a reply keeps neither prefix but its own.
    assert_eq!(
        subject("Re: the parser", ReplyMode::Forward),
        "Fwd: the parser"
    );
    // A subject that is nothing but a prefix still says something.
    assert_eq!(subject("Re:", ReplyMode::Sender), "Re: Re:");
}

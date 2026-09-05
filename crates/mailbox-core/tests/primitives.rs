//! L0: the small primitives everything else stands on — stable ids (§5.2),
//! fixture determinism (§14.1), flag normalization, modified UTF-7, IMAP
//! encodings, and trace redaction (§6.7).

use mailbox_core::clock::{format_rfc3339, parse_timestamp, Clock, Rng};
use mailbox_core::fixture::Fixture;
use mailbox_core::flags;
use mailbox_core::id::{self, Fallback};
use mailbox_core::imap::{parse_internaldate, uid_set, utf7};
use mailbox_core::session::IdRef;
use mailbox_core::trace::{redact_command, summarize_response};

// ---- §5.2: the stable id ----

fn fallback() -> Fallback<'static> {
    Fallback {
        date: 1_785_578_400,
        from: "alice@example.com",
        subject: "Invoice",
        size: 1234,
    }
}

#[test]
fn the_id_is_derived_from_the_message_not_the_server() {
    let a = id::message_id("work", Some("<x@y>"), &fallback());
    let b = id::message_id("work", Some("<x@y>"), &fallback());
    assert_eq!(a, b, "the same message must have the same id every time");
    assert_eq!(a.len(), 64);
}

/// The point of §5.2: a folder move, a UIDVALIDITY reset, and a re-download all
/// change everything the server knows and nothing the id depends on.
#[test]
fn the_id_ignores_everything_the_server_assigns() {
    let with_brackets = id::message_id("work", Some("<x@y>"), &fallback());
    let without = id::message_id("work", Some("x@y"), &fallback());
    let padded = id::message_id("work", Some("  <x@y>  "), &fallback());
    assert_eq!(with_brackets, without);
    assert_eq!(with_brackets, padded);
}

#[test]
fn different_accounts_give_different_ids() {
    assert_ne!(
        id::message_id("work", Some("<x@y>"), &fallback()),
        id::message_id("home", Some("<x@y>"), &fallback())
    );
}

#[test]
fn the_synthetic_id_is_used_only_without_a_usable_message_id() {
    let synthetic = id::message_id("work", None, &fallback());
    // An unusable Message-ID falls through to the same synthetic path, so the
    // caller need not pre-screen the header.
    assert_eq!(synthetic, id::message_id("work", Some("<>"), &fallback()));
    assert_ne!(
        synthetic,
        id::message_id("work", Some("<x@y>"), &fallback())
    );
}

#[test]
fn the_synthetic_id_depends_on_all_four_fields() {
    let base = id::message_id("work", None, &fallback());
    let mut f = fallback();
    f.date += 1;
    assert_ne!(base, id::message_id("work", None, &f));
    let mut f = fallback();
    f.from = "bob@example.com";
    assert_ne!(base, id::message_id("work", None, &f));
    let mut f = fallback();
    f.subject = "Different";
    assert_ne!(base, id::message_id("work", None, &f));
    let mut f = fallback();
    f.size += 1;
    assert_ne!(base, id::message_id("work", None, &f));
}

/// A message-id digest and a blob digest must never collide, even for the same
/// input bytes: they are different namespaces and a shared value would let one
/// be mistaken for the other.
#[test]
fn digest_namespaces_are_separated() {
    assert_ne!(
        id::blob_id(b"work"),
        id::message_id("work", None, &fallback())
    );
}

#[test]
fn blob_paths_shard_by_two_characters() {
    let blob = id::blob_id(b"hello");
    let path = id::blob_path(&blob);
    assert_eq!(&path[..2], &blob[..2]);
    assert_eq!(path.as_bytes()[2], b'/');
}

// ---- §5.2: naming a message on the command line ----

#[test]
fn short_decimal_is_a_listing_index() {
    assert_eq!(IdRef::parse("1"), IdRef::Index(1));
    assert_eq!(IdRef::parse("20"), IdRef::Index(20));
    assert_eq!(IdRef::parse("@"), IdRef::Selected);
    assert_eq!(IdRef::parse("^"), IdRef::Previous);
}

#[test]
fn an_all_digit_id_is_an_id() {
    // Hex ids are digits about one time in twenty-seven, and `ls` prints
    // seven characters of them. Reading `4014827` as an index made those
    // messages unaddressable from the command line.
    assert_eq!(
        IdRef::parse("4014827"),
        IdRef::Id("4014827".to_string()),
        "a printed id of digits must not be read as an index"
    );
    assert_eq!(
        IdRef::parse("0"),
        IdRef::Id("0".to_string()),
        "listings are numbered from one"
    );
}

// ---- §6.7: the injected clock and RNG ----

#[test]
fn a_pinned_clock_does_not_move() {
    let clock = Clock::fixed(1_788_264_000);
    assert_eq!(clock.now(), 1_788_264_000);
    clock.advance(60);
    assert_eq!(clock.now(), 1_788_264_060);
}

#[test]
fn a_seeded_rng_is_reproducible() {
    let a = Rng::seeded(b"seed").hex(16);
    let b = Rng::seeded(b"seed").hex(16);
    assert_eq!(a, b);
    assert_ne!(a, Rng::seeded(b"other").hex(16));

    // Successive draws from one Rng differ; only the sequence is pinned.
    let rng = Rng::seeded(b"seed");
    assert_ne!(rng.hex(8), rng.hex(8));
}

#[test]
fn timestamps_round_trip() {
    for ts in [0, 1_785_578_400, 2_000_000_000] {
        assert_eq!(parse_timestamp(&format_rfc3339(ts)), Some(ts));
    }
    assert_eq!(parse_timestamp("1785578400"), Some(1_785_578_400));
    assert_eq!(parse_timestamp("not a time"), None);
}

// ---- flags ----

#[test]
fn imap_flag_dialects_normalize_to_one_spelling() {
    assert_eq!(flags::normalize("\\Seen").as_deref(), Some("seen"));
    assert_eq!(flags::normalize("\\SEEN").as_deref(), Some("seen"));
    assert_eq!(flags::normalize("\\Flagged").as_deref(), Some("flagged"));
    // Junk is spelled four ways in the wild and `is:spam` means all of them.
    for junk in ["$Junk", "Junk", "$Spam", "\\Junk"] {
        assert_eq!(flags::normalize(junk).as_deref(), Some("spam"), "{junk}");
    }
    // Per-session and permitted-flags markers are not flags.
    assert_eq!(flags::normalize("\\Recent"), None);
    assert_eq!(flags::normalize("\\*"), None);
    assert_eq!(flags::normalize(""), None);
    // A keyword with whitespace would corrupt the space-delimited encoding.
    assert_eq!(flags::normalize("two words"), None);
}

/// The padding is what makes `instr(flags, ' seen ')` correct. Without it,
/// `unseen` contains `seen` and every `is:read` query is wrong.
#[test]
fn packed_flags_cannot_be_confused_with_substrings() {
    let packed = flags::pack_imap(&["\\Seen".into(), "unseen".into()]);
    assert!(flags::contains(&packed, "seen"));
    assert!(flags::contains(&packed, "unseen"));
    assert!(!flags::contains(&packed, "een"));

    let empty = flags::pack(&[]);
    assert!(!flags::contains(&empty, "seen"));
}

#[test]
fn packed_flags_are_sorted_and_deduplicated() {
    let a = flags::pack_imap(&["\\Flagged".into(), "\\Seen".into(), "\\Seen".into()]);
    let b = flags::pack_imap(&["\\Seen".into(), "\\Flagged".into()]);
    assert_eq!(a, b, "flag order must not change the stored value");
    assert_eq!(flags::unpack(&a), vec!["flagged", "seen"]);
}

// ---- modified UTF-7 (RFC 3501 §5.1.3) ----

#[test]
fn modified_utf7_round_trips() {
    for name in [
        "INBOX",
        "Archive",
        "Été",
        "日本語",
        "a&b",
        "Sent/2026",
        "&",
        "",
        "Ünïcödé/Sub",
    ] {
        let encoded = utf7::encode(name);
        assert_eq!(
            utf7::decode(&encoded),
            name,
            "round-tripping {name:?} via {encoded:?}"
        );
    }
}

#[test]
fn modified_utf7_known_vectors() {
    // From RFC 3501 §5.1.3.
    assert_eq!(
        utf7::decode("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
        "~peter/mail/台北/日本語"
    );
    assert_eq!(utf7::encode("台北"), "&U,BTFw-");
    // A literal ampersand.
    assert_eq!(utf7::decode("a&-b"), "a&b");
    assert_eq!(utf7::encode("a&b"), "a&-b");
}

/// A name we cannot decode is still a name we must be able to select, so
/// malformed input comes back rather than erroring.
#[test]
fn malformed_utf7_passes_through() {
    for bad in ["&", "&AAA", "&!!!-", "&-&"] {
        let _ = utf7::decode(bad);
    }
}

// ---- IMAP encodings ----

#[test]
fn uid_sets_compress_into_ranges() {
    assert_eq!(uid_set(&[1, 2, 3, 4, 7]), "1:4,7");
    assert_eq!(uid_set(&[5]), "5");
    assert_eq!(uid_set(&[]), "");
    assert_eq!(uid_set(&[3, 1, 2]), "1:3", "input order must not matter");
    assert_eq!(uid_set(&[1, 1, 2]), "1:2", "duplicates must not repeat");
    assert_eq!(uid_set(&[1, 3, 5]), "1,3,5");
}

#[test]
fn internaldate_parses_imaps_own_format() {
    assert_eq!(
        parse_internaldate("01-Aug-2026 10:00:00 +0000"),
        Some(1_785_578_400)
    );
    // A single-digit day is space-padded on the wire.
    assert_eq!(
        parse_internaldate(" 1-Aug-2026 10:00:00 +0000"),
        Some(1_785_578_400)
    );
    assert_eq!(
        parse_internaldate("01-Aug-2026 12:00:00 +0200"),
        Some(1_785_578_400)
    );
    assert_eq!(parse_internaldate("nonsense"), None);
}

// ---- §6.7: trace redaction ----

/// A trace is something a user attaches to a bug report. Redaction is by
/// command name rather than by pattern, because a missed pattern writes a
/// password to that file.
#[test]
fn credentials_never_reach_the_trace() {
    let redacted = redact_command("a001 LOGIN peter hunter2");
    assert!(!redacted.contains("hunter2"), "{redacted}");
    assert!(
        redacted.contains("peter"),
        "the user name is not the secret: {redacted}"
    );

    let redacted = redact_command("a002 AUTHENTICATE PLAIN AGFsaWNlAHNlY3JldA==");
    assert!(!redacted.contains("AGFsaWNl"), "{redacted}");
    assert!(redacted.contains("PLAIN"));

    // Lowercase and mixed case are the same commands.
    assert!(!redact_command("a003 login peter hunter2").contains("hunter2"));

    // Everything else passes through untouched.
    assert_eq!(redact_command("a004 SELECT INBOX"), "a004 SELECT INBOX");
}

#[test]
fn long_responses_are_summarized() {
    let long = "x".repeat(1000);
    let summary = summarize_response(&long, 64);
    assert!(summary.len() < 200);
    assert!(summary.contains("1000 bytes total"));
    assert_eq!(summarize_response("short", 64), "short");
}

// ---- §14.1: fixture determinism ----

const FIXTURE: &str = r#"
[[message]]
name = "one"
folder = "INBOX"
from = "Alice <alice@example.com>"
subject = "Hello"
body = "Hi there."
[[message.attachment]]
filename = "a.txt"
text = "attached"

[[message]]
name = "two"
folder = "INBOX"
subject = "Second"
html = "<p>rich</p>"
body = "plain"
"#;

#[test]
fn fixtures_render_deterministically() {
    let f = Fixture::parse(FIXTURE).unwrap();
    let a = f.render_all().unwrap();
    let b = f.render_all().unwrap();
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.bytes, y.bytes, "{} rendered differently twice", x.name);
    }
    // Two parses of the same text must also agree — the boundary and generated
    // Message-ID come from the message's name, not from any per-run state.
    let c = Fixture::parse(FIXTURE).unwrap().render_all().unwrap();
    assert_eq!(a[0].bytes, c[0].bytes);
}

#[test]
fn fixtures_omitting_a_date_do_not_drift() {
    let f = Fixture::parse(FIXTURE).unwrap();
    let rendered = &f.render_all().unwrap()[0];
    // A fixed epoch, not "now": a manifest without a date must render the same
    // next year.
    assert_eq!(rendered.internaldate, 1_785_060_000);
}

#[test]
fn fixture_bytes_parse_back_to_what_the_manifest_declared() {
    let f = Fixture::parse(FIXTURE).unwrap();
    let rendered = f.render_all().unwrap();

    let one = mailbox_core::mime::parse(&rendered[0].bytes);
    assert_eq!(one.subject, "Hello");
    assert_eq!(one.from[0].address, "alice@example.com");
    assert!(one.text.as_deref().unwrap_or("").contains("Hi there."));
    assert!(one.has_attachment);
    assert_eq!(one.attachments[0].filename.as_deref(), Some("a.txt"));

    let two = mailbox_core::mime::parse(&rendered[1].bytes);
    assert_eq!(two.subject, "Second");
    assert!(two.html.as_deref().unwrap_or("").contains("rich"));
    assert_eq!(two.text.as_deref().map(str::trim), Some("plain"));
}

#[test]
fn generated_attachments_are_the_size_they_claim() {
    let f = Fixture::parse(
        r#"
[[message]]
name = "big"
subject = "Big"
[[message.attachment]]
filename = "blob.bin"
generate_bytes = 10000
"#,
    )
    .unwrap();
    let rendered = &f.render_all().unwrap()[0];
    let parsed = mailbox_core::mime::parse(&rendered.bytes);
    assert_eq!(parsed.attachments[0].size, 10_000);
}

/// Duplicate names are rejected: names are how assertions refer to messages, so
/// two of them is a manifest that cannot mean one thing.
#[test]
fn duplicate_fixture_names_are_rejected() {
    let err = Fixture::parse(
        r#"
[[message]]
name = "dup"
[[message]]
name = "dup"
"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("two messages named"), "{err}");
}

/// The escape hatch for the broken-message corpus (§14.4).
#[test]
fn raw_fixtures_are_emitted_verbatim() {
    let f = Fixture::parse(
        "[[message]]\nname = \"broken\"\nraw = \"From: a@b.c\\nSubject: x\\n\\nbody\\n\"\n",
    )
    .unwrap();
    let rendered = &f.render_all().unwrap()[0];
    assert_eq!(rendered.bytes, b"From: a@b.c\r\nSubject: x\r\n\r\nbody\r\n");
}

// ---- protocol robustness (§14.4) ----

/// A response line the parser cannot read must not poison the connection.
///
/// Dovecot echoes the mailbox name into its error text, so a folder called
/// `Ärchive` produces an ordinary `NO` that happens to be 8-bit. Before this
/// recovery existed, that stranded every later command on the connection
/// waiting for a tag that had already gone past.
#[test]
fn an_unparseable_tagged_line_is_salvaged() {
    use mailbox_core::imap::connection::salvage_line;

    let salvaged = salvage_line("a0019 NO Mailbox doesn't exist: Ärchive (0.001 secs)")
        .expect("a tagged completion should be salvageable");
    match salvaged {
        imap_proto::Response::Done {
            tag,
            status,
            information,
            ..
        } => {
            assert_eq!(tag.0, "a0019");
            assert_eq!(status, imap_proto::Status::No);
            assert!(information.unwrap().contains("Ärchive"));
        }
        other => panic!("expected a tagged completion, got {other:?}"),
    }
}

#[test]
fn salvage_recognizes_every_status_and_the_continuation() {
    use mailbox_core::imap::connection::salvage_line;

    for (line, expected) in [
        ("a1 OK done", imap_proto::Status::Ok),
        ("a2 no lowercase works too", imap_proto::Status::No),
        ("a3 BAD nonsense", imap_proto::Status::Bad),
        ("a4 BYE goodbye", imap_proto::Status::Bye),
    ] {
        match salvage_line(line).expect(line) {
            imap_proto::Response::Done { status, .. } => assert_eq!(status, expected, "{line}"),
            other => panic!("{line}: got {other:?}"),
        }
    }
    assert!(matches!(
        salvage_line("+ ready"),
        Some(imap_proto::Response::Continue { .. })
    ));
}

/// Untagged junk is dropped rather than guessed at: losing an untagged response
/// degrades a sync, inventing one corrupts a mailbox.
#[test]
fn salvage_refuses_to_invent_meaning() {
    use mailbox_core::imap::connection::salvage_line;

    for line in [
        "* 5 EXISTS with Ünicode",
        "",
        "garbage",
        "a1 WAT unknown status",
        "* OK hi",
    ] {
        assert!(
            salvage_line(line).is_none(),
            "{line:?} should not be salvaged"
        );
    }
}

/// `LIST-EXTENDED` servers answer `\NonExistent` (RFC 5258) where RFC 3501
/// would say `\Noselect`, and Dovecot does exactly that under
/// `RETURN (SPECIAL-USE)`. Missing the second spelling means trying to
/// `EXAMINE` a folder that is not there, once per folder, once per pass,
/// forever.
#[test]
fn container_folders_are_not_selectable_in_either_spelling() {
    use imap_proto::NameAttribute;
    use mailbox_core::imap::is_selectable;

    assert!(is_selectable(&[]));
    assert!(is_selectable(&[NameAttribute::Marked]));
    assert!(!is_selectable(&[NameAttribute::NoSelect]));
    assert!(!is_selectable(&[NameAttribute::Extension(
        "\\NonExistent".into()
    )]));
    assert!(!is_selectable(&[NameAttribute::Extension(
        "\\nonexistent".into()
    )]));
    // An extension we do not know about is not a reason to skip a folder.
    assert!(is_selectable(&[NameAttribute::Extension(
        "\\HasChildren".into()
    )]));
}

#[test]
fn special_use_falls_back_to_the_name() {
    use mailbox_core::imap::guess_special_use;

    assert_eq!(guess_special_use("INBOX").as_deref(), Some("inbox"));
    assert_eq!(guess_special_use("Sent Items").as_deref(), Some("sent"));
    assert_eq!(guess_special_use("INBOX.Trash").as_deref(), Some("trash"));
    assert_eq!(
        guess_special_use("[Gmail]/All Mail").as_deref(),
        Some("all")
    );
    assert_eq!(guess_special_use("Archive").as_deref(), Some("archive"));
    // A conservative heuristic: a wrong guess changes which folder a message is
    // reported as living in (§5.3's ranking), so anything unrecognized stays
    // unrecognized.
    assert_eq!(guess_special_use("Projects/2026"), None);
    assert_eq!(guess_special_use("Archived Invoices"), None);
}

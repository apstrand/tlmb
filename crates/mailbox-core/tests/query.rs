//! L0: the §6.2 query grammar.
//!
//! The hard line this suite defends: **an unknown key is an error, not a silent
//! full-text fallback.** Everything else here is ordinary parser coverage; that
//! one is the difference between an agent getting no results and an agent
//! getting plausible-looking wrong ones.

use mailbox_core::clock::Clock;
use mailbox_core::query::{parse, AddrField, DateOp, Node, SizeOp};

/// 2026-09-01T12:00:00Z, so relative dates have a fixed answer.
fn clock() -> Clock {
    Clock::fixed(1_788_264_000)
}

fn p(q: &str) -> Node {
    parse(q, &clock()).unwrap_or_else(|e| panic!("`{q}` should parse: {e}"))
}

fn err(q: &str) -> String {
    match parse(q, &clock()) {
        Ok(node) => panic!("`{q}` should have failed, got {node:?}"),
        Err(e) => e.message,
    }
}

#[test]
fn empty_query_matches_everything() {
    assert_eq!(p(""), Node::All);
    assert_eq!(p("   "), Node::All);
}

#[test]
fn bare_words_are_full_text() {
    assert_eq!(p("invoice"), Node::Text("invoice".into()));
}

#[test]
fn adjacency_means_and() {
    assert_eq!(
        p("invoice receipt"),
        Node::And(vec![
            Node::Text("invoice".into()),
            Node::Text("receipt".into())
        ])
    );
}

#[test]
fn quoted_phrases_stay_whole() {
    assert_eq!(
        p("\"quarterly report\""),
        Node::Text("quarterly report".into())
    );
    assert_eq!(
        p("subject:\"quarterly report\""),
        Node::Subject("quarterly report".into())
    );
}

/// A quoted operator is a search term. Without this, "AND" is unsearchable.
#[test]
fn quoting_defeats_operators_and_keys() {
    assert_eq!(p("\"AND\""), Node::Text("AND".into()));
    assert_eq!(p("\"from:alice\""), Node::Text("from:alice".into()));
}

#[test]
fn address_fields() {
    assert_eq!(
        p("from:alice"),
        Node::Addr {
            field: AddrField::From,
            value: "alice".into()
        }
    );
    // `to:` means any recipient field — what a human means by "sent to me".
    assert_eq!(
        p("to:me"),
        Node::Addr {
            field: AddrField::Recipient,
            value: "me".into()
        }
    );
    assert_eq!(
        p("cc:bob"),
        Node::Addr {
            field: AddrField::Cc,
            value: "bob".into()
        }
    );
}

#[test]
fn flag_terms() {
    assert_eq!(
        p("is:unread"),
        Node::Flag {
            name: "seen".into(),
            want: false
        }
    );
    assert_eq!(
        p("is:read"),
        Node::Flag {
            name: "seen".into(),
            want: true
        }
    );
    assert_eq!(
        p("is:seen"),
        Node::Flag {
            name: "seen".into(),
            want: true
        }
    );
    assert_eq!(
        p("is:unseen"),
        Node::Flag {
            name: "seen".into(),
            want: false
        }
    );
    assert_eq!(
        p("is:flagged"),
        Node::Flag {
            name: "flagged".into(),
            want: true
        }
    );
    assert_eq!(
        p("is:junk"),
        Node::Flag {
            name: "spam".into(),
            want: true
        }
    );
    assert_eq!(p("has:attachment"), Node::HasAttachment);
}

#[test]
fn folder_tag_thread_account() {
    assert_eq!(p("in:inbox"), Node::Folder("inbox".into()));
    assert_eq!(p("folder:Archive"), Node::Folder("Archive".into()));
    assert_eq!(p("tag:invoices"), Node::Tag("invoices".into()));
    assert_eq!(p("account:work"), Node::Account("work".into()));
    // Angle brackets are stripped, since that is how a thread id gets pasted.
    assert_eq!(p("thread:<abc123>"), Node::Thread("abc123".into()));
}

#[test]
fn boolean_operators_and_precedence() {
    // OR binds loosest: `a AND (b OR c)` is not what `a b OR c` means.
    assert_eq!(
        p("alice invoice OR receipt"),
        Node::Or(vec![
            Node::And(vec![
                Node::Text("alice".into()),
                Node::Text("invoice".into())
            ]),
            Node::Text("receipt".into()),
        ])
    );
    assert_eq!(
        p("alice AND (invoice OR receipt)"),
        Node::And(vec![
            Node::Text("alice".into()),
            Node::Or(vec![
                Node::Text("invoice".into()),
                Node::Text("receipt".into())
            ]),
        ])
    );
}

#[test]
fn negation_both_spellings() {
    let expected = Node::Not(Box::new(Node::Flag {
        name: "spam".into(),
        want: true,
    }));
    assert_eq!(p("NOT is:spam"), expected);
    assert_eq!(p("-is:spam"), expected);
}

#[test]
fn double_negation_folds() {
    assert_eq!(p("NOT NOT invoice"), Node::Text("invoice".into()));
}

#[test]
fn the_spec_example_parses() {
    // The full example from §6.2, all at once.
    for q in [
        "from:alice to:me subject:\"quarterly report\"",
        "is:unread is:flagged has:attachment",
        "in:inbox in:archive tag:invoices thread:<abc>",
        "before:2026-08-01 after:-7d larger:5M filename:*.pdf",
        "alice AND (invoice OR receipt) NOT is:spam",
    ] {
        parse(q, &clock()).unwrap_or_else(|e| panic!("§6.2 example `{q}` failed: {e}"));
    }
}

#[test]
fn absolute_and_relative_dates() {
    assert_eq!(
        p("before:2026-08-01"),
        Node::Date {
            op: DateOp::Before,
            ts: 1_785_542_400
        }
    );
    // -7d from the pinned clock.
    assert_eq!(
        p("after:-7d"),
        Node::Date {
            op: DateOp::After,
            ts: 1_788_264_000 - 7 * 86_400
        }
    );
    // Midnight UTC, not local: a query must not mean something different in a
    // different time zone.
    assert_eq!(
        p("after:today"),
        Node::Date {
            op: DateOp::After,
            ts: 1_788_220_800
        }
    );
    assert_eq!(
        p("after:yesterday"),
        Node::Date {
            op: DateOp::After,
            ts: 1_788_220_800 - 86_400
        }
    );
    assert_eq!(
        p("after:last-week"),
        Node::Date {
            op: DateOp::After,
            ts: 1_788_220_800 - 7 * 86_400
        }
    );
    assert_eq!(
        p("before:2026-08-01T10:00:00Z"),
        Node::Date {
            op: DateOp::Before,
            ts: 1_785_578_400
        }
    );
}

#[test]
fn sizes_are_binary_multiples() {
    assert_eq!(
        p("larger:5M"),
        Node::Size {
            op: SizeOp::Larger,
            bytes: 5 * 1024 * 1024
        }
    );
    assert_eq!(
        p("smaller:500k"),
        Node::Size {
            op: SizeOp::Smaller,
            bytes: 500 * 1024
        }
    );
    assert_eq!(
        p("larger:1024"),
        Node::Size {
            op: SizeOp::Larger,
            bytes: 1024
        }
    );
    assert_eq!(
        p("larger:2GB"),
        Node::Size {
            op: SizeOp::Larger,
            bytes: 2 * 1024 * 1024 * 1024
        }
    );
}

#[test]
fn filename_globs() {
    assert_eq!(p("filename:*.pdf"), Node::Filename("*.pdf".into()));
}

#[test]
fn raw_passthrough_is_preserved() {
    let node = p("raw:HEADER X-Spam yes");
    // `raw:` takes the rest of its own token; the words after it are separate
    // terms, which is the honest reading of a space-delimited grammar.
    assert!(node.raw_terms().contains(&"HEADER"));
}

// ---- the error cases, which are the point ----

#[test]
fn unknown_key_is_an_error() {
    let msg = err("form:alice");
    assert!(msg.contains("unknown search key"), "{msg}");
    assert!(msg.contains("form"), "{msg}");
}

#[test]
fn unknown_key_suggests_the_near_miss() {
    assert!(
        err("form:alice").contains("did you mean `from:`"),
        "{}",
        err("form:alice")
    );
    assert!(err("subjekt:hi").contains("did you mean `subject:`"));
}

#[test]
fn unknown_key_error_lists_the_known_keys() {
    let msg = err("nope:1");
    for key in [
        "from", "subject", "is", "has", "before", "larger", "filename",
    ] {
        assert!(msg.contains(key), "the error should list `{key}`: {msg}");
    }
}

#[test]
fn unknown_is_value_is_an_error() {
    let msg = err("is:starred");
    assert!(msg.contains("unknown `is:` value"), "{msg}");
    assert!(
        msg.contains("flagged"),
        "the error should suggest the real values: {msg}"
    );
}

#[test]
fn unknown_has_value_is_an_error() {
    assert!(err("has:wings").contains("has:attachment"));
}

#[test]
fn malformed_values_are_errors() {
    assert!(err("before:notadate").contains("not a date"));
    assert!(err("larger:big").contains("not a size"));
    assert!(err("from:").contains("needs a value"));
}

#[test]
fn unbalanced_syntax_is_an_error() {
    assert!(err("(invoice").contains("missing ')'"));
    assert!(err("invoice)").contains("unmatched ')'"));
    assert!(err("\"unterminated").contains("unterminated"));
}

#[test]
fn parse_errors_carry_an_offset() {
    let e = parse("from:alice form:bob", &clock()).unwrap_err();
    assert_eq!(e.offset, 11, "the caret should point at the bad key");
    assert_eq!(e.unknown_key.as_deref(), Some("form"));
}

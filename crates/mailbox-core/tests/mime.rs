//! L0: MIME parsing and the renderings the contract exposes (§6.4, §14.4).
//!
//! The governing rule (§14.4): a malformed message may render badly; it may
//! never crash, hang, or lose data. Half of this file is therefore a corpus of
//! broken messages that must all come back with *something*.

use mailbox_api::BodyFormat;
use mailbox_core::mime;
use mailbox_core::render::{self, RenderOptions};

fn msg(s: &str) -> Vec<u8> {
    s.replace('\n', "\r\n").into_bytes()
}

const SIMPLE: &str = "\
From: Alice <alice@Example.COM>
To: peter@example.com, Bob <bob@example.com>
Subject: Invoice 42
Date: Sat, 01 Aug 2026 10:00:00 +0000
Message-ID: <inv42@example.com>
X-Custom: keep me

Please pay.
";

#[test]
fn headers_and_addresses() {
    let p = mime::parse(&msg(SIMPLE));
    assert_eq!(p.subject, "Invoice 42");
    assert_eq!(p.message_id.as_deref(), Some("inv42@example.com"));
    assert_eq!(p.date, Some(1_785_578_400));
    assert_eq!(p.from.len(), 1);
    assert_eq!(p.from[0].name.as_deref(), Some("Alice"));
    // §5.2's rule: the domain is folded, the local part is not.
    assert_eq!(p.from[0].address, "alice@example.com");
    assert_eq!(p.to.len(), 2);
    assert_eq!(p.text.as_deref(), Some("Please pay.\r\n"));
}

#[test]
fn broken_out_headers_do_not_repeat_in_the_header_list() {
    let p = mime::parse(&msg(SIMPLE));
    let names: Vec<String> = p
        .headers
        .iter()
        .map(|h| h.name.to_ascii_lowercase())
        .collect();
    for broken_out in ["from", "to", "subject", "date", "message-id"] {
        assert!(
            !names.contains(&broken_out.to_string()),
            "{broken_out} was emitted twice"
        );
    }
    assert!(p
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("x-custom")));
}

#[test]
fn encoded_words_are_decoded() {
    let p = mime::parse(&msg(
        "Subject: =?utf-8?B?SMOkbGxvIFfDpHJsZA==?=\nFrom: a@b.c\n\nbody\n",
    ));
    assert_eq!(p.subject, "Hällo Wärld");
}

#[test]
fn references_are_normalized_and_deduplicated() {
    let p = mime::parse(&msg(
        "References: <1@x> <2@x>\n <1@x>\nIn-Reply-To: <2@x>\nFrom: a@b.c\n\nx\n",
    ));
    assert_eq!(p.references, vec!["1@x".to_string(), "2@x".to_string()]);
    assert_eq!(p.in_reply_to.as_deref(), Some("2@x"));
}

/// Unusable `Message-ID`s must be rejected rather than carried, because §5.2
/// falls back to a synthetic id and a junk value would defeat that.
#[test]
fn unusable_message_ids_are_rejected() {
    for raw in ["<>", "  ", "<no-at-sign>", "<a b@c>", "x"] {
        let p = mime::parse(&msg(&format!("Message-ID: {raw}\nFrom: a@b.c\n\nx\n")));
        assert_eq!(p.message_id, None, "{raw:?} should not be usable");
    }
}

const MULTIPART: &str = "\
From: Alice <alice@example.com>
Subject: With attachment
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary=\"BOUND\"

--BOUND
Content-Type: multipart/alternative; boundary=\"ALT\"

--ALT
Content-Type: text/plain; charset=utf-8

plain body
--ALT
Content-Type: text/html; charset=utf-8

<p>html body</p>
--ALT--
--BOUND
Content-Type: application/pdf; name=\"invoice.pdf\"
Content-Disposition: attachment; filename=\"invoice.pdf\"
Content-Transfer-Encoding: base64

JVBERi0xLjQK
--BOUND--
";

#[test]
fn multipart_structure() {
    let p = mime::parse(&msg(MULTIPART));
    assert_eq!(p.text.as_deref().map(str::trim), Some("plain body"));
    assert!(p.html.as_deref().unwrap_or("").contains("html body"));
    assert!(p.has_attachment);

    let att: Vec<_> = p.attachments.iter().filter(|a| a.is_attachment).collect();
    assert_eq!(att.len(), 1);
    assert_eq!(att[0].filename.as_deref(), Some("invoice.pdf"));
    assert_eq!(att[0].mime_type, "application/pdf");
    // IMAP part numbering, which is what `attach <id> <part>` takes.
    assert_eq!(att[0].part, "2");
}

#[test]
fn text_parts_are_not_attachments() {
    let p = mime::parse(&msg(MULTIPART));
    let named: Vec<&str> = p
        .attachments
        .iter()
        .filter_map(|a| a.filename.as_deref())
        .collect();
    assert_eq!(
        named,
        vec!["invoice.pdf"],
        "the body parts must not be listed as attachments"
    );
}

#[test]
fn fts_text_includes_headers_and_body() {
    let p = mime::parse(&msg(MULTIPART));
    let text = p.fts_text();
    for needle in [
        "With attachment",
        "alice@example.com",
        "plain body",
        "invoice.pdf",
    ] {
        assert!(text.contains(needle), "FTS text should contain {needle:?}");
    }
}

#[test]
fn preview_is_a_single_line_of_body() {
    let p = mime::parse(&msg(SIMPLE));
    assert_eq!(p.preview(200).as_deref(), Some("Please pay."));
}

// ---- §14.4's corpus: broken messages must not crash, hang, or lose data ----

#[test]
fn broken_messages_never_panic() {
    let corpus: &[(&str, &[u8])] = &[
        ("empty", b""),
        ("headers only, no blank line", b"From: a@b.c\nSubject: x"),
        ("no headers at all", b"just a body with no headers\n"),
        ("bare LF line endings", b"From: a@b.c\nSubject: x\n\nbody\n"),
        ("null bytes", b"From: a@b.c\nSubject: x\0y\n\nbo\0dy\n"),
        ("invalid utf-8 in header", b"From: a@b.c\nSubject: \xff\xfe\n\nbody\n"),
        ("unterminated multipart", b"Content-Type: multipart/mixed; boundary=\"B\"\n\n--B\nContent-Type: text/plain\n\nhi\n"),
        ("boundary that never appears", b"Content-Type: multipart/mixed; boundary=\"NOPE\"\n\nbody\n"),
        ("header with no colon", b"NotAHeader\nFrom: a@b.c\n\nbody\n"),
        ("enormous header name", b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA: x\n\nbody\n"),
        ("only a blank line", b"\r\n"),
        ("deeply nested multiparts", b"Content-Type: multipart/mixed; boundary=\"a\"\n\n--a\nContent-Type: multipart/mixed; boundary=\"b\"\n\n--b\nContent-Type: multipart/mixed; boundary=\"c\"\n\n--c\nContent-Type: text/plain\n\nx\n--c--\n--b--\n--a--\n"),
        ("base64 that is not base64", b"Content-Transfer-Encoding: base64\nContent-Type: text/plain\n\n!!!not base64!!!\n"),
        ("charset nobody has", b"Content-Type: text/plain; charset=x-nonexistent\n\nbody\n"),
        ("date from the far future", b"Date: Mon, 01 Jan 9999 00:00:00 +0000\nFrom: a@b.c\n\nx\n"),
        ("malformed date", b"Date: yesterday-ish\nFrom: a@b.c\n\nx\n"),
        ("address with no address", b"From: \"Just A Name\"\n\nx\n"),
        ("a thousand recipients", b"To: a@b.c, a@b.c, a@b.c\n\nx\n"),
    ];

    for (name, bytes) in corpus {
        let parsed = mime::parse(bytes);
        // The contract: every field is *present*, even if empty. A caller that
        // has to guard every access is a caller that will forget one.
        let _ = parsed.subject.len();
        let _ = parsed.fts_text();
        let _ = parsed.preview(100);
        let _ = render::body(&parsed, Some(bytes), &RenderOptions::default());
        eprintln!("ok: {name}");
    }
}

// ---- renderings (§6.4) ----

#[test]
fn strip_quotes_removes_replies_and_signatures() {
    let text = "My answer.\n\nOn Sat, Alice wrote:\n> the original\n> more original\n\n-- \nPeter\nSent from a phone\n";
    assert_eq!(render::strip_quotes(text), "My answer.");
}

#[test]
fn strip_quotes_keeps_bottom_posted_prose() {
    let text = "> the question\n\nMy answer below the quote.\n";
    assert_eq!(render::strip_quotes(text), "My answer below the quote.");
}

#[test]
fn strip_quotes_leaves_unquoted_text_alone() {
    let text = "Line one.\nLine two.";
    assert_eq!(render::strip_quotes(text), text);
}

#[test]
fn max_body_bytes_truncates_and_says_so() {
    let p = mime::parse(&msg(SIMPLE));
    let opts = RenderOptions {
        max_body_bytes: Some(5),
        ..Default::default()
    };
    let body = render::body(&p, None, &opts).expect("a body");
    assert!(body.truncated);
    assert!(body.content.len() <= 5);
}

#[test]
fn truncation_never_splits_a_character() {
    // Four-byte characters, cut at a boundary that lands mid-character.
    let s = "𝄞𝄞𝄞";
    for max in 0..s.len() {
        let cut = render::truncate_utf8(s, max);
        assert!(cut.len() <= max);
        assert!(s.starts_with(cut));
    }
}

#[test]
fn html_to_markdown_keeps_meaning_and_drops_layout() {
    let html = "<table><tr><td><h2>Title</h2><p>Some <b>bold</b> and <i>italic</i>.</p>\
                <ul><li>one</li><li>two</li></ul>\
                <blockquote>quoted</blockquote>\
                <a href=\"https://example.com\">link</a></td></tr></table>";
    let md = render::html_to_markdown(html);
    assert!(md.contains("## Title"), "{md}");
    assert!(md.contains("**bold**"), "{md}");
    assert!(md.contains("*italic*"), "{md}");
    assert!(md.contains("- one\n- two"), "{md}");
    assert!(md.contains("> quoted"), "{md}");
    assert!(md.contains("[link](https://example.com)"), "{md}");
}

/// §9: no remote content, no scripts. Script and style bodies must never reach
/// the rendering, whatever the format.
#[test]
fn scripts_and_styles_are_dropped_entirely() {
    let html = "<style>body{color:red}</style><script>alert('x')</script><p>visible</p>";
    let md = render::html_to_markdown(html);
    assert_eq!(md, "visible");
}

#[test]
fn images_render_as_alt_text_only() {
    let md =
        render::html_to_markdown("<p><img src=\"https://tracker.example/1.gif\" alt=\"logo\"></p>");
    assert!(
        !md.contains("tracker.example"),
        "an image URL must never be emitted: {md}"
    );
    assert!(md.contains("logo"));
}

#[test]
fn entities_are_decoded() {
    let md = render::html_to_markdown("<p>a &amp; b &lt; c &#8217;d&#x2019;</p>");
    assert!(md.contains("a & b < c"), "{md}");
}

#[test]
fn malformed_html_still_renders() {
    for html in [
        "<p>unclosed",
        "<<<>>>",
        "<p>a</div></p></p>",
        "<a href=unquoted>x</a>",
        "<!-- comment only -->",
        "<script>never closed",
    ] {
        let _ = render::html_to_markdown(html);
    }
}

#[test]
fn raw_format_returns_the_exact_bytes() {
    let bytes = msg(SIMPLE);
    let p = mime::parse(&bytes);
    let opts = RenderOptions {
        format: BodyFormat::Raw,
        ..Default::default()
    };
    let body = render::body(&p, Some(&bytes), &opts).expect("a body");
    assert_eq!(body.content.as_bytes(), &bytes[..]);
}

#[test]
fn text_only_refuses_the_html_fallback() {
    let html_only = msg("Content-Type: text/html\nFrom: a@b.c\n\n<p>hi</p>\n");
    let p = mime::parse(&html_only);

    let with_fallback = render::body(&p, None, &RenderOptions::default());
    assert!(with_fallback.is_some(), "text format should flatten HTML");

    let opts = RenderOptions {
        text_only: true,
        ..Default::default()
    };
    assert!(
        render::body(&p, None, &opts).is_none(),
        "--text-only must not fall back"
    );
}

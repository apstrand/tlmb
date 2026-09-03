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

// ---------------------------------------------------------------------------
// Flattening HTML to text (§6.4). The property under test throughout is that
// a reader of the text form can still act on the message: the links are the
// content of most mail that arrives as HTML.
// ---------------------------------------------------------------------------

#[test]
fn links_survive_flattening_as_numbered_references() {
    let html = "<p>Your parcel is \
                <a href=\"https://track.example/9f3a?u=1\">on its way</a>, \
                see the <a href=\"https://shop.example/orders/4471\">order</a>.</p>";
    let text = render::html_to_text(html);

    assert!(text.contains("on its way [1]"), "{text}");
    assert!(text.contains("order [2]"), "{text}");
    assert!(
        text.contains("[1] https://track.example/9f3a?u=1"),
        "{text}"
    );
    assert!(
        text.contains("[2] https://shop.example/orders/4471"),
        "{text}"
    );
}

/// A newsletter links its logo, its headline, and its "view online" to one URL.
/// Three footnotes for one destination is noise.
#[test]
fn repeated_destinations_share_one_reference() {
    let html = "<a href=\"https://x.example/a\">one</a> \
                <a href=\"https://x.example/a\">two</a> \
                <a href=\"https://x.example/b\">three</a>";
    let text = render::html_to_text(html);

    assert!(text.contains("one [1]"), "{text}");
    assert!(text.contains("two [1]"), "{text}");
    assert!(text.contains("three [2]"), "{text}");
    assert_eq!(text.matches("https://x.example/a").count(), 1, "{text}");
}

/// The commonest link in real mail is one whose text is already the URL.
/// Footnoting it would print the same string twice.
#[test]
fn a_link_labelled_with_its_own_destination_is_not_doubled() {
    for html in [
        "<a href=\"https://example.com/x\">https://example.com/x</a>",
        "<a href=\"mailto:sales@example.com\">sales@example.com</a>",
    ] {
        let text = render::html_to_text(html);
        assert!(!text.contains("[1]"), "{html} produced a footnote: {text}");
        assert!(!text.contains("Links:"), "{text}");
    }
}

/// An anchor wrapped around a tracking pixel has no text to attach a marker to.
#[test]
fn a_link_with_no_text_renders_as_its_url() {
    let text = render::html_to_text("<p>before <a href=\"https://example.com/x\"></a> after</p>");
    assert!(text.contains("https://example.com/x"), "{text}");
    assert!(!text.contains("[1]"), "{text}");
}

/// §9. A `javascript:` URL must not be handed to the reader as something to
/// follow — but the words the sender wrote are still their message.
#[test]
fn executable_urls_are_dropped_and_their_text_kept() {
    for scheme in ["javascript:alert(1)", "vbscript:x", "data:text/html,<b>x"] {
        let text = render::html_to_text(&format!("<a href=\"{scheme}\">Click here</a>"));
        assert!(text.contains("Click here"), "{scheme}: {text}");
        assert!(
            !text.contains("Links:"),
            "{scheme} was offered to the reader: {text}"
        );
    }
}

/// A quoted reply is the reason `<blockquote>` appears in mail at all, and a
/// quote marker on only the first line of it is not a quote.
#[test]
fn every_line_of_a_quote_carries_its_marker() {
    let html = "<blockquote><p>first paragraph</p><p>second paragraph</p>\
                <blockquote><p>nested</p></blockquote></blockquote><p>my reply</p>";
    let text = render::html_to_text(html);

    assert!(text.contains("> first paragraph"), "{text}");
    assert!(text.contains("> second paragraph"), "{text}");
    assert!(text.contains("> > nested"), "{text}");
    assert!(
        text.lines().any(|l| l == "my reply"),
        "the reply itself must not be quoted: {text}"
    );
}

#[test]
fn an_image_is_announced_rather_than_silently_dropped() {
    let text =
        render::html_to_text("<p><img src=\"https://t.example/1.gif\" alt=\"Our logo\"></p>");
    assert!(text.contains("[image: Our logo]"), "{text}");
    assert!(!text.contains("t.example"), "§9: no image URL: {text}");

    // A mail whose whole content is one unlabelled image should not render as
    // an empty message.
    let bare = render::html_to_text("<img src=\"https://t.example/1.gif\">");
    assert_eq!(bare, "[image]");
}

/// The preview line and the FTS text want the words, not 200-character
/// tracking URLs.
#[test]
fn the_preview_flattening_keeps_no_urls() {
    let html = "<p>Hello <a href=\"https://very.long.example/track?u=1234567890\">click</a></p>";
    let plain = render::html_to_plain(html);
    assert_eq!(plain, "Hello click");
}

#[test]
fn named_entities_beyond_the_ascii_five_are_decoded() {
    let text =
        render::html_to_text("<p>1 &times; Kettle &mdash; &euro;39 &frac12; &ouml; &nbsp;x</p>");
    assert!(text.contains("1 × Kettle — €39 ½ ö"), "{text}");
    // `&nbsp;` becomes a plain space: the preview line and the search index
    // both split on whitespace, and U+00A0 is not whitespace to them.
    assert!(text.contains(" x"), "{text}");
    assert!(!text.contains('\u{a0}'), "{text}");
}

/// A bare ampersand in prose must not eat the words after it.
#[test]
fn an_ampersand_that_is_not_an_entity_survives() {
    let text = render::html_to_text("<p>Tom &amp; Jerry & friends; and more</p>");
    assert!(text.contains("Tom & Jerry & friends; and more"), "{text}");
}

// ---------------------------------------------------------------------------
// §9's sanitizer. `--format=html` is the one rendering that hands a message's
// own markup to something that will interpret it.
// ---------------------------------------------------------------------------

#[test]
fn sanitizing_removes_every_way_to_run_something() {
    let html = "<p onclick=\"alert(1)\" onmouseover=\"x()\">text</p>\
                <script>alert(2)</script>\
                <svg onload=\"alert(3)\"><circle/></svg>\
                <iframe src=\"https://evil.example\"></iframe>\
                <form action=\"https://evil.example\"><input name=\"pw\"></form>\
                <a href=\"javascript:alert(4)\">link</a>";
    let out = render::sanitize_html(html);

    for forbidden in [
        "onclick",
        "onmouseover",
        "onload",
        "<script",
        "<svg",
        "<iframe",
        "<form",
        "<input",
        "javascript:",
        "alert",
    ] {
        assert!(!out.contains(forbidden), "{forbidden} survived: {out}");
    }
    assert!(out.contains("text"), "{out}");
    assert!(out.contains("link"), "{out}");
}

/// §9: no automatic external loads. The URL is kept but disarmed, because
/// "load remote content for this sender" has to have something to load.
#[test]
fn remote_images_are_disarmed_but_recoverable() {
    let out = render::sanitize_html("<img src=\"https://tracker.example/o.gif?id=42\" alt=\"a\">");
    // The attribute itself, not the substring inside `data-blocked-src`.
    assert!(!out.contains(" src=\""), "a remote src survived: {out}");
    assert!(
        out.contains("data-blocked-src=\"https://tracker.example/o.gif?id=42\""),
        "{out}"
    );
    assert!(out.contains("alt=\"a\""), "{out}");
}

/// An inline part carries its own bytes: nothing is fetched and nobody learns
/// the message was opened.
#[test]
fn inline_parts_keep_their_content_id() {
    let out = render::sanitize_html("<img src=\"cid:logo@ex\" alt=\"logo\">");
    assert!(out.contains("data-cid=\"logo@ex\""), "{out}");
}

/// `style` is the attribute everyone assumes is inert. `background: url(…)` is
/// a remote load and a read receipt, and it needs no script.
#[test]
fn the_style_attribute_does_not_survive() {
    let out =
        render::sanitize_html("<p style=\"background:url(https://tracker.example/p.gif)\">hi</p>");
    assert!(!out.contains("style"), "{out}");
    assert!(!out.contains("tracker.example"), "{out}");
    assert!(out.contains("hi"), "{out}");
}

#[test]
fn document_furniture_never_becomes_prose() {
    let out = render::sanitize_html(
        "<head><title>Subject line</title><style>b{}</style></head><body><p>real</p></body>",
    );
    assert!(!out.contains("Subject line"), "{out}");
    assert!(!out.contains("b{}"), "{out}");
    assert!(out.contains("real"), "{out}");
}

/// Mail HTML is full of stray closers. One that closes an element we never
/// opened would hand the surrounding document's structure to the sender.
#[test]
fn tags_are_balanced_whatever_the_sender_sent() {
    let out = render::sanitize_html("</div></blockquote><blockquote>quote<div>x");
    assert_eq!(out.matches("<blockquote>").count(), 1, "{out}");
    assert_eq!(out.matches("</blockquote>").count(), 1, "{out}");
    assert!(out.ends_with("</div></blockquote>"), "{out}");
}

/// An unknown element loses its tags, never its words: a message whose content
/// sits inside something we have not heard of should read as prose rather than
/// vanish.
#[test]
fn unknown_elements_keep_their_text() {
    let out = render::sanitize_html("<custom-card><p>inside</p></custom-card>");
    assert!(out.contains("inside"), "{out}");
    assert!(!out.contains("custom-card"), "{out}");
}

#[test]
fn sanitizing_never_panics_on_broken_markup() {
    for html in [
        "<p>unclosed",
        "<<<>>>",
        "<a href=unquoted>x</a>",
        "<!-- comment",
        "<script>never closed",
        "<p</p>",
        "&#xZZZZ; &#99999999999;",
        "<img src=\"cid:\">",
    ] {
        let _ = render::sanitize_html(html);
        let _ = render::html_to_text(html);
    }
}

/// The table is binary-searched, so an unsorted entry would silently stop
/// decoding. Cheap to assert, impossible to notice otherwise.
#[test]
fn the_entity_table_is_sorted() {
    let mut previous = "";
    for name in render::entity_names() {
        assert!(
            previous < name,
            "{previous} then {name}: table is out of order"
        );
        previous = name;
    }
}

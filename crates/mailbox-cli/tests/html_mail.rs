//! Reading mail that arrived as HTML, end to end (§6.4, §9).
//!
//! Most mail is HTML now, and a client that flattens it to prose has to decide
//! what to do with the parts of the message that are not prose. The decision
//! this file pins down: the links survive. A rendering of an order
//! confirmation whose "track your parcel" leads nowhere is a summary of that
//! mail, not a rendering of it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A message with everything the flattener has to make a decision about: a
/// linked image, links whose text differs from their destination, a link whose
/// text *is* its destination, a `mailto:`, a URL written out in the prose, an
/// executable URL, a quoted reply, entities, and a `text/plain` alternative
/// that is empty — which is how a great many senders ship HTML mail.
const FIXTURE: &str = r#"
[[folder]]
path = "INBOX"
special_use = "inbox"

[[message]]
name = "shipped"
folder = "INBOX"
from = "Example Shop <ship@shop.example>"
to = ["peter@example.com"]
subject = "Your order has shipped"
date = "2026-08-30T09:00:00Z"
message_id = "<ship4471@shop.example>"
html = """
<html><head><title>Order shipped</title><style>b{color:red}</style></head><body>
<a href="https://shop.example/?utm=news"><img src="https://cdn.shop.example/logo.png" alt="Example Shop"></a>
<h2>Your order has shipped</h2>
<p>You can <a href="https://track.example/p/9f3a?ref=email">track it here</a>,
or view the <a href="https://shop.example/orders/4471">order details</a>.</p>
<ul><li>1 &times; Kettle &mdash; &euro;39.00</li></ul>
<p>Write to <a href="mailto:support@shop.example">support@shop.example</a>.</p>
<blockquote><p>On Fri, Peter wrote:</p><p>When does this ship?</p>
<p>I ordered last week.</p></blockquote>
<p><a href="javascript:alert(1)">Claim your prize</a></p>
<p>Or go to https://shop.example/help directly.</p>
</body></html>
"""
"#;

struct Cli {
    dir: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl Cli {
    fn new() -> Cli {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mailbox-html-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            "[accounts.test]\naddress = \"peter@example.com\"\nimap_host = \"127.0.0.1\"\n",
        )
        .expect("write config");

        let cli = Cli {
            state: dir.join("state"),
            config,
            dir,
        };
        let manifest = cli.dir.join("fixture.toml");
        std::fs::write(&manifest, FIXTURE).expect("write fixture");
        let seeded = cli.run(&["debug", "fixture", "load", manifest.to_str().unwrap()]);
        assert_eq!(seeded.status, 0, "seeding failed: {}", seeded.stderr);
        cli
    }

    fn run(&self, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_mailbox"))
            .args(args)
            .env("MAILBOX_CONFIG", &self.config)
            .env("MAILBOX_STATE_DIR", &self.state)
            .env("MAILBOX_CLOCK", "2026-09-01T12:00:00Z")
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("COLUMNS", "100")
            .env("NO_COLOR", "1")
            .env_remove("MAILBOX_AGENT")
            .output()
            .expect("run the binary");
        Run {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// The only message in the fixture.
    fn id(&self) -> String {
        let listing = self.run(&["--json", "ls", "-n", "10"]).json();
        listing["messages"][0]["id"]
            .as_str()
            .expect("a message")
            .to_string()
    }
}

struct Run {
    status: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("expected one JSON document, got {e}:\n{}", self.stdout))
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn an_html_only_message_renders_with_its_links() {
    let cli = Cli::new();
    let id = cli.id();
    let shown = cli.run(&["show", &id]);
    assert_eq!(shown.status, 0, "{}", shown.stderr);
    let body = &shown.stdout;

    // The prose, with each link marked where it stood. The numbering is
    // reading order, so the linked logo above the headline takes [1].
    assert!(body.contains("[image: Example Shop] [1]"), "{body}");
    assert!(body.contains("track it here [2]"), "{body}");
    assert!(body.contains("order details [3]"), "{body}");

    // And the destinations, once, under the message.
    assert!(
        body.contains("[2] https://track.example/p/9f3a?ref=email"),
        "{body}"
    );
    assert!(
        body.contains("[3] https://shop.example/orders/4471"),
        "{body}"
    );

    // A link whose text is already its destination is not doubled.
    assert_eq!(body.matches("support@shop.example").count(), 1, "{body}");

    // §9: an executable URL is never offered as something to follow, though
    // the words the sender wrote remain their message.
    assert!(body.contains("Claim your prize"), "{body}");
    assert!(!body.contains("javascript:"), "{body}");

    // The quoted reply keeps its marker on every line of it.
    assert!(body.contains("> When does this ship?"), "{body}");
    assert!(body.contains("> I ordered last week."), "{body}");

    // Entities are characters, not their source text.
    assert!(body.contains("1 × Kettle — €39.00"), "{body}");

    // §9: no image is fetched, and the alt text says one was there.
    assert!(!body.contains("cdn.shop.example"), "{body}");

    // The document's furniture is not prose.
    assert!(!body.contains("color:red"), "{body}");
}

/// An empty `text/plain` alternative is a placeholder, not a rendering. Taking
/// it at its word displays a blank page beside a message whose whole content
/// sits in the part next to it.
#[test]
fn an_empty_text_alternative_does_not_win_over_the_html() {
    let cli = Cli::new();
    let id = cli.id();
    let body = cli.run(&["show", &id]).stdout;
    assert!(
        body.contains("Your order has shipped"),
        "the body rendered empty: {body}"
    );
}

/// §6.4: the links are data, so an agent can act on one without parsing a
/// rendering back into a list.
#[test]
fn the_links_are_in_the_json_in_the_order_the_text_numbers_them() {
    let cli = Cli::new();
    let id = cli.id();
    let shown = cli.run(&["--json", "show", &id]).json();
    let links = shown["message"]["body"]["links"]
        .as_array()
        .expect("body.links")
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        links,
        vec![
            "https://shop.example/?utm=news",
            "https://track.example/p/9f3a?ref=email",
            "https://shop.example/orders/4471",
            // Written out in the prose rather than marked up, and still
            // something the reader can be handed.
            "https://shop.example/help",
        ],
        "{shown:#}"
    );
}

#[test]
fn links_can_be_listed_on_their_own() {
    let cli = Cli::new();
    let id = cli.id();
    let out = cli.run(&["show", &id, "--links"]).stdout;

    assert!(
        out.starts_with("[1] https://shop.example/?utm=news"),
        "{out}"
    );
    assert_eq!(out.lines().count(), 4, "{out}");
}

/// §9: `--format=html` is the one rendering handed to something that will
/// interpret it, and the sender's markup must not reach that intact.
#[test]
fn the_html_rendering_is_sanitized() {
    let cli = Cli::new();
    let id = cli.id();
    let html = cli.run(&["show", &id, "--format=html"]).stdout;

    for forbidden in ["<style", "<script", "javascript:", "<title", "color:red"] {
        assert!(!html.contains(forbidden), "{forbidden} survived:\n{html}");
    }
    // The remote image is disarmed, not deleted: "load remote content for this
    // sender" needs something left to load.
    assert!(!html.contains(" src=\""), "{html}");
    assert!(
        html.contains("data-blocked-src=\"https://cdn.shop.example/logo.png\""),
        "{html}"
    );
    assert!(html.contains("<h2>Your order has shipped</h2>"), "{html}");
}

/// The preview line is one line: the URLs would crowd out the words.
#[test]
fn the_preview_carries_no_urls() {
    let cli = Cli::new();
    let listing = cli.run(&["--json", "ls", "-n", "10"]).json();
    let preview = listing["messages"][0]["preview"]
        .as_str()
        .expect("a preview")
        .to_string();

    assert!(!preview.contains("http"), "{preview}");
    assert!(preview.contains("Your order has shipped"), "{preview}");
}

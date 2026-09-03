//! Opening a link from a message (§9).
//!
//! The rule this file exists to hold: a message is untrusted input, and the
//! only thing it may cause is a web page to open — never a program to run,
//! never a file to be handed to a registered handler.

use mailbox_core::open::{can_open, open_url};

#[test]
fn only_web_and_mail_links_are_opened() {
    for url in [
        "https://example.com/x",
        "http://example.com",
        "mailto:someone@example.com",
        "MAILTO:someone@example.com",
    ] {
        assert!(can_open(url), "{url} should be openable");
    }
}

/// `xdg-open` dispatches on scheme to whatever handler is registered, and some
/// of those handlers execute. The question is not which schemes are dangerous
/// today but which two a link in mail legitimately is.
#[test]
fn everything_else_is_refused() {
    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,<script>x</script>",
        "smb://server/share",
        "ssh://host",
        "vbscript:x",
        "chrome://settings",
        // No scheme at all: this is where a leading `-` would be read as a
        // flag by whatever we hand it to.
        "-version",
        "./local/file",
        "example.com",
        "",
        "https:",
    ] {
        assert!(!can_open(url), "{url} must not be openable");
        assert!(open_url(url).is_err(), "{url} must not be opened");
    }
}

/// A URL carrying a newline could make the command line say something other
/// than what the reader saw.
#[test]
fn control_characters_are_refused() {
    assert!(!can_open("https://example.com/\n--flag"));
    assert!(!can_open("https://example.com/\r\nx"));
    assert!(!can_open("https://example.com/\u{0}"));
}

/// The URL reaches the opener as one argument to one program — no shell, so
/// nothing in it is interpreted. Observed by pointing `MAILBOX_OPENER` at a
/// script, which is also how a user on an unusual desktop configures this.
#[test]
fn the_url_is_passed_as_a_single_argument() {
    let dir = std::env::temp_dir().join(format!("mailbox-open-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("opener.sh");
    let record = dir.join("opened.txt");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$1\" > {}\n",
            record.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // A URL full of the characters a shell would act on.
    let url = "https://example.com/a?x=1&y=$(id);z=`whoami`|cat 'q'";
    std::env::set_var("MAILBOX_OPENER", &script);
    let result = open_url(url);
    std::env::remove_var("MAILBOX_OPENER");
    result.expect("the opener should have run");

    let written = std::fs::read_to_string(&record).unwrap();
    let mut lines = written.lines();
    assert_eq!(lines.next(), Some("1"), "exactly one argument: {written}");
    assert_eq!(lines.next(), Some(url), "unmodified: {written}");

    // And a failing opener is reported rather than swallowed: a UI that says
    // "opened" when nothing opened is worse than one that says why not.
    //
    // Checked here rather than in its own test because both halves set a
    // process-wide variable, and the test binary is threaded.
    std::env::set_var("MAILBOX_OPENER", "/nonexistent/opener");
    let failure = open_url("https://example.com").unwrap_err().to_string();
    std::env::remove_var("MAILBOX_OPENER");
    assert!(failure.contains("/nonexistent/opener"), "{failure}");

    std::fs::remove_dir_all(&dir).ok();
}

//! Handing a URL to the desktop's own browser.
//!
//! §9 draws a hard line at attachments — "Attachments are never auto-opened" —
//! and this respects the same line for links. Nothing here happens on its own:
//! a URL is opened because someone asked for that URL, having seen it. The
//! checks below exist so that "someone asked" cannot be turned into something
//! else by the sender of a message.
//!
//! Like notifications (§11), it runs the platform's own command rather than
//! linking a desktop library, which keeps a platform dependency out of the
//! static binary and makes the whole thing testable by pointing `MAILBOX_OPENER`
//! at a script.

use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// Schemes a message is allowed to send someone to.
///
/// An allowlist, and a very short one. `xdg-open` will cheerfully dispatch
/// `file:///`, `smb://`, and whatever else has a handler registered — including
/// handlers that execute — so the question is not "is this scheme dangerous"
/// but "did the user ask for a web page or an email". Those are the two things
/// a link in mail legitimately is.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// Opens `url` in the desktop's default handler.
///
/// The URL is passed as one argument to one program: no shell, so no quoting
/// question and nothing to escape. `MAILBOX_OPENER` overrides the command,
/// which is how the tests observe this without a browser.
pub fn open_url(url: &str) -> Result<()> {
    let url = url.trim();
    check(url)?;

    let opener = std::env::var("MAILBOX_OPENER").unwrap_or_else(|_| default_opener().to_string());
    let mut parts = opener.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| Error::usage("MAILBOX_OPENER is empty"))?;

    let status = Command::new(program)
        .args(parts)
        .arg(url)
        // The opener's own chatter is not ours to print into a TUI that is
        // about to repaint over it.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| Error::usage(format!("could not run `{program}`: {e}")))?;

    if !status.success() {
        return Err(Error::usage(format!(
            "`{program}` exited with {status}; set MAILBOX_OPENER to the command that opens a URL here"
        )));
    }
    Ok(())
}

/// Whether [`open_url`] would accept this URL, for a UI that wants to grey out
/// what it cannot do rather than offer it and fail.
pub fn can_open(url: &str) -> bool {
    check(url.trim()).is_ok()
}

fn check(url: &str) -> Result<()> {
    // A URL that begins with `-` is read as a flag by whatever we hand it to.
    // Requiring a scheme rules that out, and rules out relative paths with it.
    let Some((scheme, rest)) = url.split_once(':') else {
        return Err(Error::usage(format!(
            "`{url}` has no scheme; only {} links can be opened",
            ALLOWED_SCHEMES.join(", ")
        )));
    };
    if !ALLOWED_SCHEMES.contains(&scheme.to_ascii_lowercase().as_str()) {
        return Err(Error::usage(format!(
            "refusing to open a `{scheme}:` link; only {} are opened from mail (§9)",
            ALLOWED_SCHEMES.join(", ")
        )));
    }
    if rest.is_empty() {
        return Err(Error::usage("that link has no destination"));
    }
    // A control character in a URL is either an encoding accident or an attempt
    // to make the command line say something other than what was displayed.
    if url.chars().any(|c| c.is_control()) {
        return Err(Error::usage("that link contains control characters"));
    }
    Ok(())
}

fn default_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "cmd /c start"
    } else {
        "xdg-open"
    }
}

//! Putting text on the system clipboard, through the terminal itself.
//!
//! OSC 52 rather than `wl-copy`/`xclip`/`pbcopy`: the escape sequence is
//! written to the terminal, and the terminal — which is the thing that actually
//! owns a clipboard — does the rest. That is the same reasoning as the keychain
//! (§9) and notifications (§11), and it buys three things a helper process
//! cannot. It works over ssh, where the clipboard is at the other end of the
//! connection and no local tool can reach it. It works under tmux and screen,
//! which forward the sequence. And it adds no Wayland, X11, or macOS dependency
//! to a binary §4 wants self-contained.
//!
//! The cost is that a terminal may refuse: reading and writing the clipboard is
//! a real capability, and some terminals disable OSC 52 by default. There is no
//! reply to the sequence, so this cannot tell — which is why the caller says
//! "copied" only in the sense of "sent to your terminal", and why the URL stays
//! on screen where it can be selected by hand.

use std::io::Write;

use base64::Engine;

use crate::error::Result;

/// Sends `text` to the terminal's clipboard.
///
/// Written to the given stream — the terminal, in practice — rather than to
/// stdout unconditionally, so a TUI can hand it the handle it already owns and
/// a test can hand it a buffer.
pub fn copy(out: &mut impl Write, text: &str) -> Result<()> {
    // `c` is the selection: the clipboard proper, not the X11 primary
    // selection, since "copy" in every other application means this one.
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(())
}

/// The sequence itself, for tests and for callers that write their own output.
pub fn sequence(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

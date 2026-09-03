//! OSC 52, the clipboard escape (§7's `y`).

use mailbox_core::clipboard;

#[test]
fn the_sequence_is_the_terminals_clipboard_escape() {
    let mut out: Vec<u8> = Vec::new();
    clipboard::copy(&mut out, "https://example.com/x").unwrap();

    // `]52;c;` is "set the clipboard selection", then base64, then BEL.
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b]52;c;aHR0cHM6Ly9leGFtcGxlLmNvbS94\x07"
    );
}

/// A URL is arbitrary bytes as far as a terminal is concerned. Base64 is what
/// keeps one from carrying its own escape sequence into the stream.
#[test]
fn nothing_in_the_text_reaches_the_terminal_uninterpreted() {
    let hostile = "https://x/\x07\x1b]52;c;bad\x07\x1b[2J";
    let sequence = clipboard::sequence(hostile);

    // One BEL: the one that terminates our own sequence.
    assert_eq!(sequence.matches('\x07').count(), 1, "{sequence:?}");
    assert_eq!(sequence.matches('\x1b').count(), 1, "{sequence:?}");
    assert!(sequence.starts_with("\x1b]52;c;"));
}

#[test]
fn non_ascii_survives_the_round_trip() {
    use base64::Engine;
    let text = "https://example.com/søk?q=naïve&x=日本";
    let sequence = clipboard::sequence(text);
    let payload = sequence
        .trim_start_matches("\x1b]52;c;")
        .trim_end_matches('\x07');

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("valid base64");
    assert_eq!(String::from_utf8(decoded).unwrap(), text);
}

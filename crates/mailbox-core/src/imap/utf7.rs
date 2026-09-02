//! Modified UTF-7 (RFC 3501 §5.1.3), the encoding IMAP folder names use.
//!
//! It is UTF-7 with two changes: `&` is the shift character instead of `+`, and
//! `/` in the base64 alphabet is `,`. Servers that speak IMAP4rev2 or advertise
//! `UTF8=ACCEPT` send UTF-8 directly, but plenty do not, and a mailbox called
//! `&AMk-t&AOk-` in the folder list is a bug report waiting to happen.

/// Decodes a modified-UTF-7 folder name. Input that is not valid modified UTF-7
/// comes back unchanged — a name we cannot decode is still a name we must be
/// able to select.
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'&' {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // `&-` is a literal ampersand.
        if i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            out.push('&');
            i += 2;
            continue;
        }
        let Some(end) = bytes[i + 1..].iter().position(|b| *b == b'-') else {
            // Unterminated shift: emit the rest verbatim rather than dropping it.
            out.push_str(&s[i..]);
            break;
        };
        let chunk = &s[i + 1..i + 1 + end];
        match decode_b64_utf16(chunk) {
            Some(decoded) => out.push_str(&decoded),
            None => out.push_str(&s[i..i + 2 + end]),
        }
        i += end + 2;
    }
    out
}

/// Encodes a folder name back to modified UTF-7, for `SELECT` on a server that
/// has not accepted UTF-8.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending: Vec<u16> = Vec::new();

    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        out.push('&');
        out.push_str(&encode_b64_utf16(pending));
        out.push('-');
        pending.clear();
    };

    for c in s.chars() {
        if c == '&' {
            flush(&mut pending, &mut out);
            out.push_str("&-");
        } else if ('\u{20}'..='\u{7e}').contains(&c) {
            flush(&mut pending, &mut out);
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

fn decode_b64_utf16(chunk: &str) -> Option<String> {
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut units: Vec<u16> = Vec::new();

    for c in chunk.bytes() {
        let v = ALPHABET.iter().position(|a| *a == c)? as u32;
        bits = (bits << 6) | v;
        nbits += 6;
        if nbits >= 16 {
            nbits -= 16;
            units.push(((bits >> nbits) & 0xffff) as u16);
        }
    }
    // Leftover bits must be zero padding, not a truncated code unit.
    if nbits >= 6 || (bits & ((1 << nbits) - 1)) != 0 {
        return None;
    }
    String::from_utf16(&units).ok()
}

fn encode_b64_utf16(units: &[u16]) -> String {
    let mut bits: u32 = 0;
    let mut nbits = 0u32;
    let mut out = String::new();
    for u in units {
        bits = (bits << 16) | u32::from(*u);
        nbits += 16;
        while nbits >= 6 {
            nbits -= 6;
            out.push(ALPHABET[((bits >> nbits) & 0x3f) as usize] as char);
        }
    }
    if nbits > 0 {
        out.push(ALPHABET[((bits << (6 - nbits)) & 0x3f) as usize] as char);
    }
    out
}

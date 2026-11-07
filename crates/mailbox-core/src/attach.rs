//! Attachments: listing them, and getting them onto disk safely (§6.1, §9).
//!
//! §9 is specific about extraction, and the reasons are worth keeping in view:
//!
//! > Attachments are never auto-opened; `attach --extract` writes with the
//! > executable bit cleared and mangles dangerous extensions.
//!
//! A filename in a message is attacker-controlled. It can contain a path, a
//! traversal, a NUL, a newline, or an extension the desktop will happily
//! execute. Everything here treats it as hostile.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Extensions that get an extra suffix on the way to disk.
///
/// Not a virus scanner and not pretending to be one. The list is the small set
/// a double-click on a desktop will *run*, which is the difference between an
/// attachment being a file and being a program.
const DANGEROUS: &[&str] = &[
    "exe", "com", "bat", "cmd", "scr", "pif", "msi", "msp", "hta", "cpl", "jar", "vb", "vbs",
    "vbe", "js", "jse", "wsf", "wsh", "ps1", "psm1", "reg", "lnk", "scf", "inf", "dll", "so",
    "dylib", "app", "command", "desktop", "sh", "bash", "zsh", "py", "rb", "pl",
];

/// Makes a filename from a message safe to write.
///
/// Keeps the name recognisable — a user looking for `invoice.pdf` should find
/// something called `invoice.pdf` — while removing every way it could be
/// something other than a file in the directory the user named.
pub fn safe_filename(raw: &str) -> String {
    // Only the last segment, so `../../.ssh/authorized_keys` becomes
    // `authorized_keys` rather than an escape.
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_start_matches('.');

    let mut cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                c
            }
        })
        .collect();

    if cleaned.is_empty() {
        cleaned = "attachment".to_string();
    }
    // Long enough for any real name, short enough for every filesystem — and
    // cut on a character, not a byte, or a long non-ASCII name panics here.
    truncate_chars(&mut cleaned, 200);

    // A dangerous extension is suffixed rather than stripped: the user can
    // still tell what it was, and can still rename it deliberately, but a
    // double-click does not run it.
    let extension = cleaned
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if DANGEROUS.contains(&extension.as_str()) {
        cleaned.push_str(".bin");
    }
    cleaned
}

/// What a saved message is called: its subject, as an `.eml` file.
///
/// `.eml` because that is what every mail client and most desktops open as a
/// message, and the bytes are exactly RFC 5322 — what the server sent. The
/// subject is cut short first, so a long one cannot push the extension off
/// the end of [`safe_filename`]'s limit.
pub fn message_filename(subject: &str) -> String {
    let mut stem: String = subject
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&mut stem, 120);
    let stem = stem.trim_end_matches(['.', ' ']);
    let stem = if stem.is_empty() { "message" } else { stem };
    safe_filename(&format!("{stem}.eml"))
}

fn truncate_chars(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Where an extracted part should be written, without overwriting anything.
///
/// A message cannot be allowed to overwrite a file by choosing its name, so a
/// collision gets a counter rather than clobbering.
pub fn destination(dir: &Path, filename: &str) -> PathBuf {
    let safe = safe_filename(filename);
    let candidate = dir.join(&safe);
    if !candidate.exists() {
        return candidate;
    }

    let (stem, extension) = match safe.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (safe.clone(), String::new()),
    };
    for n in 1..1000 {
        let candidate = dir.join(format!("{stem}-{n}{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}{extension}", std::process::id()))
}

/// Where a typed save path really points.
///
/// A directory — one that exists, or anything ending in `/` — gets
/// `filename` inside it, with [`destination`]'s counter on a collision. A
/// file path is taken as it is, but never over a file already there: the
/// prompt that asked was a line of text, not a dialog that warned. `~/` is
/// the home directory, as it is everywhere else someone types a path.
pub fn save_target(typed: &str, filename: &str) -> Result<PathBuf> {
    let typed = typed.trim();
    let path = match typed
        .strip_prefix("~/")
        .or(if typed == "~" { Some("") } else { None })
    {
        Some(rest) => match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => PathBuf::from(home).join(rest),
            None => return Err(Error::usage("there is no home directory to put it in")),
        },
        None => PathBuf::from(typed),
    };
    if typed.ends_with('/') || path.is_dir() {
        if !path.is_dir() {
            return Err(Error::not_found("directory", path.display().to_string()));
        }
        return Ok(destination(&path, filename));
    }
    if path.exists() {
        return Err(Error::usage(format!(
            "{} already exists — give it another name",
            path.display()
        )));
    }
    Ok(path)
}

/// Writes an extracted part, with the executable bit cleared (§9).
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(Error::not_found("directory", parent.display().to_string()));
        }
    }
    std::fs::write(path, bytes)?;
    clear_executable_bit(path)?;
    Ok(())
}

#[cfg(unix)]
fn clear_executable_bit(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    // Owner read/write only. An attachment is not a program, and it is not
    // other users' business either.
    perms.set_mode(perms.mode() & 0o666 & !0o111);
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn clear_executable_bit(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_is_cut_on_a_character() {
        let name = format!("{}.pdf", "å".repeat(150));
        let safe = safe_filename(&name);
        assert!(safe.len() <= 200);
        assert!(safe.starts_with('å'));
    }

    #[test]
    fn a_typed_path_is_a_file_or_a_directory_and_never_clobbers() {
        let dir = std::env::temp_dir().join(format!("mailbox-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let typed = dir.display().to_string();

        assert_eq!(save_target(&typed, "a.eml").unwrap(), dir.join("a.eml"));
        assert_eq!(
            save_target(&format!("{typed}/b.txt"), "a.eml").unwrap(),
            dir.join("b.txt")
        );
        std::fs::write(dir.join("a.eml"), b"x").unwrap();
        assert_eq!(save_target(&typed, "a.eml").unwrap(), dir.join("a-1.eml"));
        assert!(save_target(&format!("{typed}/a.eml"), "a.eml").is_err());
        assert!(save_target(&format!("{typed}/nope/"), "a.eml").is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_message_is_named_for_its_subject() {
        assert_eq!(message_filename("Re: Lunch?"), "Re_ Lunch_.eml");
        assert_eq!(message_filename("  \t "), "message.eml");
        assert_eq!(message_filename("../../etc/passwd"), "passwd.eml");
        assert_eq!(message_filename("two\r\nlines"), "two lines.eml");
        let long = message_filename(&"ö".repeat(300));
        assert!(long.ends_with(".eml") && long.len() <= 200, "{long}");
    }
}

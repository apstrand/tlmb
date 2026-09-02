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
    // Long enough for any real name, short enough for every filesystem.
    if cleaned.len() > 200 {
        cleaned.truncate(200);
    }

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

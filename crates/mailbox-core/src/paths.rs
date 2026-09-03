//! Where things live, and the environment variables that relocate them.
//!
//! `MAILBOX_STATE_DIR` and `MAILBOX_CONFIG` (§6.7) move *every* path the
//! process touches. That is what lets the test suite run in parallel with no
//! shared state and no `$HOME` pollution — and it only works if nothing else in
//! the codebase ever builds a path from `$HOME` directly.

use std::path::PathBuf;

/// `$MAILBOX_CONFIG`, else `$XDG_CONFIG_HOME/mailbox/config.toml`, else
/// `~/.config/mailbox/config.toml`.
pub fn config_file() -> PathBuf {
    if let Some(p) = env_path("MAILBOX_CONFIG") {
        return p;
    }
    config_dir().join("config.toml")
}

pub fn config_dir() -> PathBuf {
    if let Some(p) = env_path("MAILBOX_CONFIG") {
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }
    xdg("XDG_CONFIG_HOME", ".config").join("mailbox")
}

/// `$MAILBOX_STATE_DIR`, else `$XDG_DATA_HOME/mailbox`.
pub fn state_dir() -> PathBuf {
    if let Some(p) = env_path("MAILBOX_STATE_DIR") {
        return p;
    }
    xdg("XDG_DATA_HOME", ".local/share").join("mailbox")
}

/// The per-account store of §5.1.
pub fn account_dir(account_id: &str) -> PathBuf {
    state_dir().join(account_id)
}

/// `$XDG_RUNTIME_DIR/mailbox/<account>.sock` — the owner socket of §4.1.
///
/// Falls back to the state directory where `XDG_RUNTIME_DIR` is unset, which is
/// the normal case on macOS and in containers.
pub fn owner_socket(account_id: &str) -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("mailbox"),
        _ => state_dir().join("run"),
    };
    base.join(format!("{account_id}.sock"))
}

/// The advisory lock file that decides ownership of an account (§4.1).
pub fn owner_lock(account_id: &str) -> PathBuf {
    account_dir(account_id).join("owner.lock")
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    if let Some(p) = env_path(var) {
        return p;
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(fallback),
        // No HOME at all: use the working directory rather than writing to `/`.
        None => PathBuf::from(".").join(fallback),
    }
}

/// What this machine calls itself, for the places where a device has to be
/// named to the user.
///
/// Two of those exist so far: the pairing confirmation (§8.3), where it tells
/// you which machine is offering you credentials, and a snooze (§7), where it
/// tells you which machine will wake the message — a snooze is local until M8,
/// and naming the holder is how the interface says so rather than implying a
/// sync that is not happening.
///
/// `MAILBOX_DEVICE` overrides it: tests need a stable name, and a hostname like
/// `arch-vm-3` is not what anyone calls their laptop.
pub fn device_name() -> String {
    if let Ok(name) = std::env::var("MAILBOX_DEVICE") {
        let name = name.trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this device".to_string())
}

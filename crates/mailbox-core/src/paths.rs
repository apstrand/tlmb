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

/// Relocates every per-account path for this process.
///
/// `SessionOptions.state_dir` is how the mobile shells point the store at the
/// app's own data directory, because XDG does not exist on a phone. Everything
/// *derived* from that directory — the owner socket, the OAuth2 token cache —
/// has to move with it, and a function that reads the environment cannot know
/// it moved. So the relocation is recorded here, once, by whoever opens the
/// session.
///
/// Process-wide because the setting is: it is where this application keeps its
/// data, not a per-command choice.
pub fn set_state_root(root: &std::path::Path) {
    let mut slot = state_root().lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(root.to_path_buf());
}

fn state_root() -> &'static std::sync::Mutex<Option<PathBuf>> {
    static ROOT: std::sync::OnceLock<std::sync::Mutex<Option<PathBuf>>> =
        std::sync::OnceLock::new();
    ROOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// [`set_state_root`], else `$MAILBOX_STATE_DIR`, else `$XDG_DATA_HOME/mailbox`.
pub fn state_dir() -> PathBuf {
    if let Some(root) = state_root()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return root;
    }
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
    #[cfg(target_arch = "wasm32")]
    {
        return "this browser".to_string();
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
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
}

/// Writes a file only its owner can read.
///
/// The mode is set in `open` rather than with a `chmod` afterwards: a chmod
/// leaves a window in which the file is world-readable, and the files that go
/// through here are exactly what something else on the machine is looking for.
///
/// Used for the config (which may carry an OAuth2 client secret), the token
/// cache, and anything pairing writes.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;

    // An existing file keeps the mode it already had, so tighten it too —
    // a config written before this existed is still readable by everyone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

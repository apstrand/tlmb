//! The session-scoped listing history behind `^` and `1..20` (§5.2).
//!
//! These conveniences exist for a human at a prompt, and they need state that
//! outlives one process — a CLI invocation is over before the next one starts.
//! It lives in the runtime directory rather than the store: it is scratch, it
//! is per-terminal-session in spirit, and losing it on reboot is correct.
//!
//! Agents should use ids and are warned when they do not (§6.4); the warning
//! belongs to the frontend, but the reason it is possible at all is that this
//! is the only path by which an index becomes an id.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    /// Full ids, in the order the last listing printed them.
    #[serde(default)]
    pub listing: Vec<String>,
}

impl History {
    fn path(account: &str) -> PathBuf {
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v).join("mailbox"),
            _ => crate::paths::state_dir().join("run"),
        };
        base.join(format!("{account}.listing.json"))
    }

    pub fn load(account: &str) -> History {
        std::fs::read_to_string(History::path(account))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Records what a command just printed. Failures are ignored: a convenience
    /// that cannot be saved must not fail the command that produced the output.
    pub fn save(account: &str, ids: &[String]) {
        let path = History::path(account);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let h = History {
            listing: ids.to_vec(),
        };
        if let Ok(text) = serde_json::to_string(&h) {
            let _ = std::fs::write(path, text);
        }
    }

    /// `^` — the most recent message printed. The *first* row of the last
    /// listing, because listings are newest-first and "the last message I saw"
    /// means the one at the top.
    pub fn previous(&self) -> Option<String> {
        self.listing.first().cloned()
    }

    /// `1..20` — one-based, as printed.
    pub fn index(&self, n: usize) -> Option<String> {
        self.listing.get(n.checked_sub(1)?).cloned()
    }
}

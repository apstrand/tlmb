//! `state.toml` — the non-secret sync bookkeeping of §5.1: `UIDVALIDITY`,
//! `HIGHESTMODSEQ`, and what the last pass managed to finish.
//!
//! It lives beside `meta.db` rather than inside it, and the ordering between
//! the two is deliberate: **`meta.db` commits first, `state.toml` second.**
//! A crash in between leaves bookkeeping that under-claims — it says we have
//! less than we do — so the next pass re-fetches a delta it already applied.
//! Applying a delta twice is a no-op (content-addressed blobs, upsert by
//! stable id); *missing* one is silent data loss. The safe direction is
//! therefore to under-claim, and this ordering is what guarantees it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Per-folder sync position.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FolderState {
    /// The server's `UIDVALIDITY`. A change means every UID we hold for this
    /// folder is meaningless and the folder must be rescanned (§5.3).
    pub uidvalidity: u32,
    /// The `UIDNEXT` seen at the end of the last pass — the cheap "is there
    /// anything new" check before a `SEARCH`.
    #[serde(default)]
    pub uidnext: u32,
    /// `HIGHESTMODSEQ` from `CONDSTORE`, so reconnect is a delta rather than a
    /// rescan. Zero when the server does not offer `CONDSTORE`.
    #[serde(default)]
    pub highestmodseq: u64,
    /// The largest UID we have successfully stored. Distinct from `uidnext`:
    /// it is what we have, not what the server will assign next.
    #[serde(default)]
    pub max_uid: u32,
    /// Unix seconds of the last pass that completed for this folder.
    #[serde(default)]
    pub last_sync: i64,

    /// The oldest UID this folder has been filled in down to (§5.1).
    ///
    /// A first sync of a twenty-year mailbox works newest-first in chunks, so
    /// the inbox is usable in seconds and the archive arrives behind it. This
    /// is the low-water mark of that walk: everything at or above it has been
    /// fetched, and everything below is still on the server only.
    ///
    /// Zero means nothing has been backfilled yet — which for a folder that
    /// has never synced is the same as "all of it is still to come".
    #[serde(default)]
    pub backfill_uid: u32,

    /// True when every message in the folder is in the store.
    ///
    /// Tracked rather than derived from `backfill_uid <= 1`: UIDs are sparse,
    /// so the lowest one that exists is not 1 and a walk that has reached the
    /// bottom cannot be recognised by arithmetic.
    #[serde(default)]
    pub complete: bool,

    /// How many messages the server says are in this folder, from the last
    /// pass. What the progress in `status` is a fraction *of*.
    #[serde(default)]
    pub server_total: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncState {
    /// The account this store belongs to. Present so a misplaced directory is
    /// caught on open rather than silently synced into.
    #[serde(default)]
    pub account: String,
    /// Unix seconds of the last completed pass over every folder.
    #[serde(default)]
    pub last_full_sync: i64,
    /// Capabilities the server advertised on the last connection, so the CLI
    /// can explain what path a sync took without reconnecting.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Keyed by folder path as the server spells it. A `BTreeMap` so the file
    /// is byte-stable across writes — a state file that reorders itself makes
    /// every diff noise and every golden comparison useless.
    #[serde(default)]
    pub folders: BTreeMap<String, FolderState>,
}

impl SyncState {
    pub fn path(store_dir: &Path) -> PathBuf {
        store_dir.join("state.toml")
    }

    /// Reads the file, or returns a fresh state when it does not exist.
    ///
    /// A *corrupt* file is not fatal either: it is bookkeeping, and discarding
    /// it costs one full rescan, which is strictly better than refusing to
    /// start.
    pub fn load(store_dir: &Path) -> SyncState {
        let path = SyncState::path(store_dir);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return SyncState::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    /// Writes atomically. Call *after* the `meta.db` transaction has committed.
    pub fn save(&self, store_dir: &Path) -> Result<()> {
        let path = SyncState::path(store_dir);
        let text = toml::to_string_pretty(self)
            .map_err(|e| crate::Error::store(format!("serializing state.toml: {e}")))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn folder(&self, path: &str) -> FolderState {
        self.folders.get(path).cloned().unwrap_or_default()
    }

    pub fn set_folder(&mut self, path: &str, state: FolderState) {
        self.folders.insert(path.to_string(), state);
    }

    /// Folders that still have older mail to fetch, newest-first.
    pub fn backfilling(&self) -> Vec<(&str, &FolderState)> {
        self.folders
            .iter()
            .filter(|(_, f)| !f.complete && f.uidvalidity != 0)
            .map(|(path, state)| (path.as_str(), state))
            .collect()
    }

    /// Drops bookkeeping for folders the server no longer lists, so a renamed
    /// folder does not keep a stale `UIDVALIDITY` forever.
    pub fn retain_folders(&mut self, present: &[String]) {
        self.folders.retain(|k, _| present.iter().any(|p| p == k));
    }
}

//! Who is allowed to be the sync engine (§4.1).
//!
//! > Exactly one process per account may be the sync engine.
//!
//! The reason is in §4.1 and it is not about SQLite. SQLite serializes the
//! individual writes correctly; what it cannot do is stop two sync engines
//! interleaving into a coherent database full of incoherent mail — duplicated
//! messages, a `HIGHESTMODSEQ` asserting deltas that were never applied, an
//! outbox entry sent twice. The transaction boundary that matters spans
//! `meta.db`, the blob store, and the `UIDVALIDITY`/`HIGHESTMODSEQ`
//! bookkeeping, and no database enforces that for us.
//!
//! It matters more now than it did. A single `sync --once` racing another was
//! a narrow window; a background loop that holds a connection for hours is a
//! process someone will still have running when they open the TUI tomorrow.
//!
//! What this is *not* is §4.1's whole design. The Unix socket that lets the
//! losing processes have their writes performed by the owner is still to come;
//! until it does, a loser is told plainly rather than queued behind anything.
//! Reads never come here at all — they open the store read-only and skip
//! ownership entirely, which is the fast path for scripts and agents.

#[cfg(not(target_arch = "wasm32"))]
use std::fs::{File, OpenOptions};
#[cfg(not(target_arch = "wasm32"))]
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::error::{Error, Result};

/// Proof that this process is the sync engine for one account.
///
/// The lock is held by the open file descriptor, so it is released when this
/// value is dropped — and also if the process dies, is killed, or panics.
/// That last part is the reason for `flock` rather than a pid file: a crashed
/// owner leaves a stale pid file and a mailbox nobody will sync until someone
/// deletes it by hand.
#[derive(Debug)]
pub struct Owner {
    #[cfg(not(target_arch = "wasm32"))]
    _file: File,
    account: String,
}

impl Owner {
    /// Takes ownership of an account, or reports who has it.
    ///
    /// Never waits. A caller who cannot be the sync engine wants to know now,
    /// so it can say so; blocking would turn "the TUI is running" into "this
    /// command hangs".
    #[cfg(not(target_arch = "wasm32"))]
    pub fn acquire(store_dir: &Path, account: &str) -> Result<Owner> {
        std::fs::create_dir_all(store_dir)?;
        let path = store_dir.join("owner.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;

        // SAFETY: `flock` takes a file descriptor and a flag, and this one is
        // open for the call's duration. `LOCK_NB` makes it return rather than
        // wait.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                // Worth saying what happens next, because for the common case
                // — a flag or a move made while something else is syncing —
                // the answer is "nothing is lost". Local writes are already in
                // the store and the operation queue; the process that holds
                // the lock replays them on its next pass (§5.3).
                return Err(Error::Conflict(format!(
                    "another mailbox process is already syncing {account}, and only one may be \
                     (§4.1). Anything you have already changed is queued and will be pushed by \
                     that process; there is nothing to redo."
                )));
            }
            return Err(Error::store(format!(
                "could not lock {}: {error}",
                path.display()
            )));
        }

        // Whose it is, for a human looking at the directory. Advisory only —
        // nothing reads this back, because a pid in a file is a fact about the
        // past and the lock is a fact about the present.
        use std::io::Write;
        let mut file = file;
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        let _ = file.flush();

        Ok(Owner {
            _file: file,
            account: account.to_string(),
        })
    }

    /// A browser worker is already the single owner of its in-memory store.
    /// Stage 2 will replace this placeholder with OPFS-backed ownership.
    #[cfg(target_arch = "wasm32")]
    pub fn acquire(_store_dir: &Path, account: &str) -> Result<Owner> {
        Ok(Owner {
            account: account.to_string(),
        })
    }

    /// Takes ownership if it is free, and says so rather than failing if not.
    pub fn try_acquire(store_dir: &Path, account: &str) -> Result<Option<Owner>> {
        match Owner::acquire(store_dir, account) {
            Ok(owner) => Ok(Some(owner)),
            Err(Error::Conflict(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }
}

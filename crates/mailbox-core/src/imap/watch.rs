//! The background sync loop (§5.3).
//!
//! `sync_once` has always been *the* unit of work; this is the thing that runs
//! it more than once. What it adds is the waiting: `IDLE` where the server
//! offers it, a poll where it does not, and backoff where the network is gone.
//!
//! > `IDLE` per selected folder plus a bounded pool of idling folders
//! > (default: inbox and any folder viewed in the last 24 h); poll fallback
//! > with backoff.
//!
//! This is the first half: one idled folder, the inbox, which is where the mail
//! a person is waiting for arrives. A pool wants a connection per folder and a
//! thread to hold each, and is worth doing after the single case has been run
//! against real servers rather than before.
//!
//! # Why the waiting is bounded
//!
//! RFC 2177 requires a client to re-issue `IDLE` at least every 29 minutes, and
//! the middleboxes between a laptop and a mail server are less patient than
//! that. So the wait always ends, and ending it is a full round trip — `DONE`,
//! the tagged completion, then a sync pass if anything happened. A loop that
//! idles forever is a loop that stops receiving mail an hour in and does not
//! notice.
//!
//! # What it does not do
//!
//! It does not decide that it may run. [`crate::owner::Owner`] does that, and
//! the caller must hold one: two sync engines on one account produce a
//! coherent database full of incoherent mail (§4.1).

use std::time::Duration;

use mailbox_api::SyncResult;

use crate::error::{Error, Result};
use crate::imap::connection::Idle;
use crate::imap::sync::SyncOptions;
use crate::trace::event;
use crate::Workspace;

/// How the loop was woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// The first pass, before any waiting.
    Started,
    /// The server said something during `IDLE`.
    Server,
    /// The idle window closed, or the poll interval elapsed.
    Elapsed,
    /// The previous pass failed and this is the retry.
    Retry,
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    pub sync: SyncOptions,
    /// The longest single `IDLE`. RFC 2177's ceiling is 29 minutes; the
    /// default is well under it because a NAT that drops an idle connection
    /// does so silently, and the cost of finding out is one reconnect.
    pub idle_window: Duration,
    /// How often to sync when the server does not offer `IDLE`.
    pub poll_interval: Duration,
    /// The first backoff after a failure. It doubles, to `max_backoff`.
    pub backoff: Duration,
    pub max_backoff: Duration,
    /// Stop after this many passes. For tests; `None` runs until interrupted.
    pub max_passes: Option<usize>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        WatchOptions {
            sync: SyncOptions::default(),
            idle_window: Duration::from_secs(9 * 60),
            poll_interval: Duration::from_secs(5 * 60),
            backoff: Duration::from_secs(15),
            max_backoff: Duration::from_secs(15 * 60),
            max_passes: None,
        }
    }
}

/// Runs sync passes until stopped, waiting between them.
///
/// `on_pass` is called after every pass, successful or not, and is where a
/// frontend prints, notifies, or repaints. It gets the workspace because
/// announcing new mail means reading what arrived and marking it announced —
/// and it returns whether to keep going, so a caller can stop on a condition
/// of its own without this module knowing what the condition is.
pub fn watch(
    workspace: &mut Workspace,
    opts: &WatchOptions,
    mut on_pass: impl FnMut(&mut Workspace, Wake, &Result<SyncResult>) -> bool,
) -> Result<()> {
    let mut wake = Wake::Started;
    let mut passes = 0usize;
    let mut backoff = opts.backoff;

    loop {
        let result = workspace.sync(&opts.sync);
        passes += 1;

        let failed = result.is_err();
        let keep_going = on_pass(workspace, wake, &result);
        if !keep_going || opts.max_passes.is_some_and(|max| passes >= max) {
            return Ok(());
        }

        if failed {
            // A failed pass is usually the network, and the network usually
            // comes back. Backing off rather than retrying immediately is what
            // keeps a laptop with the lid shut from making a thousand
            // connections to a server that is refusing them.
            if let Ok(session) = workspace.primary() {
                session.tracer.emit(
                    event::SYNC_BACKOFF,
                    serde_json::json!({ "seconds": backoff.as_secs() }),
                );
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(opts.max_backoff);
            wake = Wake::Retry;
            continue;
        }
        backoff = opts.backoff;

        wake = match wait(workspace, opts) {
            Ok(Wake::Server) => Wake::Server,
            Ok(other) => other,
            // A failed wait is a failed connection; the next pass will find
            // out properly and report it through `on_pass`.
            Err(_) => Wake::Retry,
        };
    }
}

/// Waits for something to happen, or for the window to close.
fn wait(workspace: &mut Workspace, opts: &WatchOptions) -> Result<Wake> {
    // `IDLE` selects one mailbox on one connection, so it can only serve one
    // account. With several, this polls them all — which is correct, just less
    // prompt, and `--account` narrows it back to the idling path.
    if workspace.sessions().len() != 1 {
        std::thread::sleep(opts.poll_interval);
        return Ok(Wake::Elapsed);
    }

    let session = workspace.primary()?;
    let mut client = session.connect()?;

    if !client.has("IDLE") {
        let _ = client.logout();
        std::thread::sleep(opts.poll_interval);
        return Ok(Wake::Elapsed);
    }

    // The inbox: where the mail someone is waiting for arrives, and the folder
    // every server has.
    let inbox = session
        .store()
        .folders()?
        .into_iter()
        .find(|f| f.special_use.as_deref() == Some("inbox"))
        .map(|f| f.path)
        .unwrap_or_else(|| "INBOX".to_string());

    client.select(&inbox)?;
    let outcome = client.idle(opts.idle_window);
    let _ = client.logout();

    match outcome? {
        Idle::Woken => Ok(Wake::Server),
        Idle::TimedOut => Ok(Wake::Elapsed),
    }
}

/// Whether an error is one the loop should keep going after.
///
/// Everything to do with the network is; a configuration mistake or a refused
/// password is not, because retrying it forever produces an authentication
/// failure every fifteen minutes and no mail.
pub fn is_transient(error: &Error) -> bool {
    matches!(error, Error::Network(_) | Error::Timeout(_))
}

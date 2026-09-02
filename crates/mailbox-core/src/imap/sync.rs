//! The read sync engine (§5.3).
//!
//! One pass to quiescence, driven by [`sync_once`] — which is not a test hook
//! bolted on afterwards but *the* unit of work (§6.7). The background loop of
//! later milestones will call this in a loop; every end-to-end test steps the
//! engine with it; and because the whole pass is one function with no ambient
//! timers, "what did this sync do" has an answer.
//!
//! Three paths, best first:
//!
//! 1. **`QRESYNC`** — the server tells us what vanished and what changed since
//!    our `HIGHESTMODSEQ`. One round trip, no scan.
//! 2. **`CONDSTORE`** — the server tells us what changed, but not what
//!    vanished, so expunges still need a UID list.
//! 3. **Neither** — `UID SEARCH ALL` and diff. Correct everywhere, and the
//!    reason a bare IMAP4rev1 server is supported rather than tolerated.

use std::collections::{BTreeSet, HashMap};

use mailbox_api::SyncReport;

use crate::clock::Clock;
use crate::error::Result;
use crate::imap::{Client, Fetched};
use crate::store::ingest::Incoming;
use crate::store::{FolderState, Store};
use crate::trace::event;

/// How many bodies one `--full` pass will fetch per folder.
const FULL_FETCH_CHUNK: usize = 100_000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncOptions {
    /// `sync --full`: fetch every body, for offline use (§5.1).
    pub full: bool,
    /// `sync --folder`: restrict the pass. Paths as the server spells them.
    pub folders: Option<Vec<String>>,
    /// `sync --since`: only look at messages newer than this instant.
    ///
    /// Note the consequence, which the engine honours: a windowed pass cannot
    /// detect expunges, because it never sees the UIDs outside the window.
    pub since: Option<i64>,
    /// The body prefetch window in days (§5.1's default: 30). `None` fetches no
    /// bodies eagerly.
    pub prefetch_days: Option<i64>,
    /// A ceiling on eager body fetches per folder per pass, so a first sync of
    /// a large mailbox does not become a full download.
    pub max_prefetch: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions {
            full: false,
            folders: None,
            since: None,
            prefetch_days: Some(30),
            max_prefetch: 500,
        }
    }
}

/// Runs exactly one sync pass to quiescence and returns.
///
/// No background loop, no `IDLE`, no timers (§6.7). If the tracer was armed
/// with `--until`, the pass stops at the next folder boundary after that event.
pub fn sync_once(
    client: &mut Client,
    store: &mut Store,
    opts: &SyncOptions,
    clock: &Clock,
) -> Result<SyncReport> {
    let started = std::time::Instant::now();
    // An L1 recording is of a pass made with particular options (§14.2), and
    // replaying it with different ones would ask for bytes the recording does
    // not contain. Recording them makes the replay ask the same questions.
    client.record_sync_options(opts);

    let tracer = store.tracer().clone();
    tracer.emit(
        event::SYNC_BEGIN,
        serde_json::json!({
            "account": store.account(),
            "full": opts.full,
            "qresync": client.qresync(),
        }),
    );

    let mut report = SyncReport {
        account: store.account().to_string(),
        ..Default::default()
    };

    // Push before pull (§5.3). A pull that ran first would fetch the server's
    // pre-edit state, overwrite the user's local change, and only then push it.
    let replayed = crate::imap::replay::replay(client, store)?;
    report.ops_applied = replayed.applied;
    report.ops_deferred = replayed.deferred;
    report.ops_conflicted = replayed.conflicts;
    let mut state = store.sync_state();
    state.account = store.account().to_string();
    state.capabilities = client.capabilities().iter().cloned().collect();

    // 1. Folders first: a message cannot be placed in a folder we have not
    //    recorded, and a folder that vanished server-side must stop being
    //    synced before we try to select it.
    let discovered = client.list_folders()?;
    let mut present: Vec<String> = Vec::with_capacity(discovered.len());
    for rec in &discovered {
        store.upsert_folder(rec)?;
        present.push(rec.path.clone());
    }
    for (id, path, _) in store.folder_sync_order()? {
        if !present.contains(&path) {
            // The messages go with the folder, and they count as removed: a
            // report that said "0 removed" while the mailbox shrank would make
            // `sync --json` useless for exactly the case a caller most wants to
            // notice.
            report.messages_removed += store.clear_folder_placements(id)? as u32;
            store.delete_folder(&path)?;
            tracer.emit1(event::SYNC_FOLDER_END, "folder_deleted", &path);
        }
    }
    state.retain_folders(&present);

    // 2. Then each folder, inbox first.
    for (folder_id, path, _rank) in store.folder_sync_order()? {
        if let Some(only) = &opts.folders {
            if !only.iter().any(|f| f.eq_ignore_ascii_case(&path)) {
                continue;
            }
        }
        if tracer.stop_requested() {
            break;
        }

        tracer.emit1(event::SYNC_FOLDER_BEGIN, "folder", &path);
        let folder_state = state.folder(&path);
        let result = sync_folder(client, store, folder_id, &path, &folder_state, opts, clock);

        match result {
            Ok(outcome) => {
                report.folders_examined += 1;
                report.messages_added += outcome.added;
                report.messages_updated += outcome.updated;
                report.messages_removed += outcome.removed;
                report.bodies_fetched += outcome.bodies;
                if outcome.uidvalidity_reset {
                    report.uidvalidity_resets.push(path.clone());
                }
                state.set_folder(&path, outcome.state);
                tracer.emit(
                    event::SYNC_FOLDER_END,
                    serde_json::json!({
                        "folder": path,
                        "added": outcome.added,
                        "updated": outcome.updated,
                        "removed": outcome.removed,
                        "bodies": outcome.bodies,
                    }),
                );
            }
            Err(e) => {
                // One unselectable or broken folder must not abort the pass:
                // the inbox is usually the folder the user cares about, and it
                // is usually not the broken one. It is not silent, though —
                // see the check after the loop.
                report.folders_failed += 1;
                report.errors.push(mailbox_api::FolderError {
                    folder: path.clone(),
                    error: e.to_string(),
                });
                tracer.emit(
                    event::SYNC_FOLDER_END,
                    serde_json::json!({ "folder": path, "error": e.to_string() }),
                );
            }
        }
    }

    // A pass in which *every* folder failed is a failed pass. Reporting
    // success because the loop completed would tell an agent the mailbox is up
    // to date when nothing was fetched at all — the one lie a sync must never
    // tell.
    if report.folders_examined == 0 && report.folders_failed > 0 {
        let first = report
            .errors
            .first()
            .map(|e| format!("{}: {}", e.folder, e.error))
            .unwrap_or_else(|| "every folder failed".to_string());
        tracer.emit1(event::SYNC_END, "failed", &first);
        return Err(crate::Error::Network(first));
    }

    store.refresh_folder_counts()?;
    state.last_full_sync = clock.now();
    // §5.1's ordering: meta.db has already committed; this is the write that is
    // allowed to be lost. See `store::state` for why that direction is safe.
    store.save_sync_state(&state)?;

    report.duration_ms = started.elapsed().as_millis() as u64;
    tracer.emit(
        event::SYNC_END,
        serde_json::to_value(&report).unwrap_or_default(),
    );
    Ok(report)
}

#[derive(Debug, Default)]
struct FolderOutcome {
    added: u32,
    updated: u32,
    removed: u32,
    bodies: u32,
    uidvalidity_reset: bool,
    state: FolderState,
}

fn sync_folder(
    client: &mut Client,
    store: &mut Store,
    folder_id: i64,
    path: &str,
    prev: &FolderState,
    opts: &SyncOptions,
    clock: &Clock,
) -> Result<FolderOutcome> {
    let tracer = store.tracer().clone();
    let mut outcome = FolderOutcome::default();

    let qresync_from = (prev.uidvalidity > 0 && prev.highestmodseq > 0)
        .then_some((prev.uidvalidity, prev.highestmodseq));
    let selected = client.examine(path, qresync_from)?;

    // A UIDVALIDITY change invalidates every UID we hold (§5.3). The messages
    // themselves survive the rescan, because their ids are derived from their
    // content rather than from the UID (§5.2).
    if prev.uidvalidity != 0 && selected.uidvalidity != prev.uidvalidity {
        tracer.emit(
            event::SYNC_UIDVALIDITY_RESET,
            serde_json::json!({
                "folder": path,
                "was": prev.uidvalidity,
                "now": selected.uidvalidity,
            }),
        );
        outcome.removed += store.clear_folder_placements(folder_id)? as u32;
        outcome.uidvalidity_reset = true;
    }

    let used_qresync = client.qresync() && qresync_from.is_some() && !outcome.uidvalidity_reset;
    let known = store.placement_uids(folder_id)?;
    let max_uid = store.max_uid(folder_id)?;

    // What the server volunteered as part of the select, plus — where it did
    // not volunteer anything — what CONDSTORE says changed.
    let mut changed: Vec<Fetched> = selected.changed.clone();
    if !used_qresync
        && !outcome.uidvalidity_reset
        && prev.highestmodseq > 0
        && client.has("CONDSTORE")
    {
        changed = client.fetch_changed_since(prev.highestmodseq)?;
    }

    // Vanished UIDs, when the server told us. Otherwise expunges are found by
    // diffing a full UID listing further down.
    if used_qresync {
        for uid in &selected.vanished {
            if store.remove_placement(folder_id, *uid)? {
                outcome.removed += 1;
            }
        }
    }

    // The changed set mixes flag updates to messages we have with messages we
    // have never seen; only the latter need a header fetch.
    let mut new_uids: BTreeSet<u32> = BTreeSet::new();
    for f in &changed {
        if known.contains(&f.uid) {
            if store.set_placement_flags(folder_id, f.uid, &f.flags, f.modseq)? {
                outcome.updated += 1;
            }
        } else {
            new_uids.insert(f.uid);
        }
    }

    // The slow path: no usable MODSEQ, so ask the server what is there.
    //
    // A `--since` window is honoured here, and it is exactly why the expunge
    // diff below is skipped in that case: a windowed listing says nothing about
    // the UIDs outside it, and treating its absence as an expunge would delete
    // the user's older mail.
    //
    // A `UIDVALIDITY` reset lands here too, and must: we have just dropped
    // every placement in the folder, so the incremental tail-fetch below would
    // start from UID 0 and ask for nothing. The rescan a reset promises (§5.3)
    // *is* this full listing.
    let mut listed_everything = false;
    let needs_full_listing = !used_qresync
        && (outcome.uidvalidity_reset || prev.highestmodseq == 0 || !client.has("CONDSTORE"));
    if needs_full_listing {
        let criteria = match opts.since {
            Some(ts) => format!("SINCE {}", imap_date(ts)),
            None => "ALL".to_string(),
        };
        listed_everything = opts.since.is_none();
        let server_uids = client.uid_search(&criteria)?;
        for uid in &server_uids {
            if !known.contains(uid) {
                new_uids.insert(*uid);
            }
        }
        if listed_everything {
            let keep: BTreeSet<u32> = server_uids.iter().copied().collect();
            outcome.removed += store.retain_uids(folder_id, &keep)? as u32;
        } else {
            // Even in a window, flags on messages we already hold are worth
            // refreshing — it is one cheap FETCH and it is what makes
            // `sync --since` useful for "catch me up" rather than misleading.
            let refresh: Vec<u32> = server_uids
                .iter()
                .copied()
                .filter(|u| known.contains(u))
                .collect();
            for f in client.fetch_flags(&refresh)? {
                if store.set_placement_flags(folder_id, f.uid, &f.flags, f.modseq)? {
                    outcome.updated += 1;
                }
            }
        }
    } else if !used_qresync {
        // CONDSTORE told us about changes but says nothing about arrivals with
        // a modseq the server never reported to us; a tail fetch covers them
        // cheaply.
        if selected.uidnext > max_uid + 1 {
            for uid in client.uid_search(&format!("UID {}:*", max_uid + 1))? {
                if !known.contains(&uid) {
                    new_uids.insert(uid);
                }
            }
        }
    }

    // Fetch and store the new messages.
    if !new_uids.is_empty() {
        let uids: Vec<u32> = new_uids.iter().copied().collect();
        let fetched = client.fetch_headers(&uids)?;
        let batch: Vec<Incoming> = fetched
            .iter()
            .filter_map(|f| {
                // A message with no header block is one the server could not
                // give us; skipping it leaves the UID unknown, so the next pass
                // tries again rather than recording a hole.
                let headers = f.headers.clone()?;
                Some(Incoming {
                    folder_id,
                    uid: f.uid,
                    flags: f.flags.clone(),
                    internaldate: if f.internaldate > 0 {
                        f.internaldate
                    } else {
                        clock.now()
                    },
                    size: f.size,
                    modseq: f.modseq,
                    headers,
                    raw: None,
                })
            })
            .collect();
        let applied = store.apply_batch(&batch)?;
        outcome.added += applied.iter().filter(|a| a.is_new_placement).count() as u32;
        tracer.emit(
            event::SYNC_MESSAGES_APPLIED,
            serde_json::json!({ "folder": path, "count": applied.len() }),
        );
    }

    // Bodies: everything with `--full`, the recent window otherwise (§5.1).
    let want_bodies = opts.full || opts.prefetch_days.is_some();
    if want_bodies {
        let since = if opts.full {
            None
        } else {
            // Saturating, so a preposterous window is a wide one rather than an
            // underflow that wraps into the future and matches nothing.
            opts.prefetch_days
                .map(|d| clock.now().saturating_sub(d.saturating_mul(86_400)))
        };
        // `--full` still takes a ceiling per pass: a mailbox with a million
        // messages should make progress on every run rather than open one
        // fetch that either finishes in an hour or dies.
        let limit = if opts.full {
            FULL_FETCH_CHUNK
        } else {
            opts.max_prefetch
        };
        let pending = store.uids_needing_body(folder_id, since, limit)?;
        if !pending.is_empty() {
            let by_uid: HashMap<u32, String> = pending.iter().cloned().collect();
            let uids: Vec<u32> = pending.iter().map(|(u, _)| *u).collect();
            for f in client.fetch_bodies(&uids)? {
                let (Some(raw), Some(id)) = (f.body.as_ref(), by_uid.get(&f.uid)) else {
                    continue;
                };
                store.set_body(id, raw)?;
                outcome.bodies += 1;
            }
            tracer.emit(
                event::SYNC_BODIES_FETCHED,
                serde_json::json!({ "folder": path, "count": outcome.bodies }),
            );
        }
    }

    outcome.state = FolderState {
        uidvalidity: selected.uidvalidity,
        uidnext: selected.uidnext,
        // Only claim a MODSEQ when this pass actually reconciled the whole
        // folder against it. Claiming one after a windowed pass would make the
        // next pass believe it had already seen everything older.
        highestmodseq: if listed_everything || used_qresync || opts.since.is_none() {
            selected.highestmodseq
        } else {
            prev.highestmodseq
        },
        max_uid: store.max_uid(folder_id)?,
        last_sync: clock.now(),
    };
    Ok(outcome)
}

/// IMAP's `SEARCH` date format: `01-Aug-2026`.
pub fn imap_date(ts: i64) -> String {
    let fmt = time::macros::format_description!("[day]-[month repr:short]-[year]");
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|d| d.format(fmt).ok())
        .unwrap_or_else(|| "01-Jan-1970".to_string())
}

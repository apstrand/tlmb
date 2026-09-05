//! `mailbox watch` (§6.4): the mailbox as a stream of events.
//!
//! The point is the loop an agent writes around it — "when mail arrives, do
//! something" — without polling `ls` and diffing the result. Everything that
//! makes that safe is in the store's change feed rather than here: events are
//! durable, cursored, and written in the same transaction as the change they
//! describe, so a consumer that dies mid-event resumes without missing one.
//!
//! This command is the feed's reader, plus one convenience: with nobody else
//! keeping the mailbox up to date, it becomes the sync engine itself. A watch
//! that streams only what this device did would be a strange thing to hand an
//! agent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mailbox_core::error::{Error, Result};
use mailbox_core::store::changes::ChangeKind;
use mailbox_core::Workspace;

use crate::cli::WatchArgs;
use crate::commands::Ctx;
use crate::output::DIM;

/// Where each account's feed has been read to. Opaque to the caller, which is
/// what §6.4 asks of a cursor — but one seq per account, because a workspace is
/// several stores and a single number could only ever be right about one.
#[derive(Debug, Clone, Default)]
struct Cursor(Vec<(String, i64)>);

impl Cursor {
    fn parse(s: &str) -> Result<Cursor> {
        let mut out = Vec::new();
        for part in s.split(',').filter(|p| !p.is_empty()) {
            let (account, seq) = part
                .rsplit_once(':')
                .ok_or_else(|| Error::usage(format!("`{s}` is not a watch cursor")))?;
            let seq: i64 = seq
                .parse()
                .map_err(|_| Error::usage(format!("`{s}` is not a watch cursor")))?;
            out.push((account.to_string(), seq));
        }
        Ok(Cursor(out))
    }

    fn get(&self, account: &str) -> Option<i64> {
        self.0
            .iter()
            .find(|(a, _)| a == account)
            .map(|(_, seq)| *seq)
    }

    fn set(&mut self, account: &str, seq: i64) {
        match self.0.iter_mut().find(|(a, _)| a == account) {
            Some(slot) => slot.1 = seq,
            None => self.0.push((account.to_string(), seq)),
        }
    }

    fn encode(&self) -> String {
        self.0
            .iter()
            .map(|(a, seq)| format!("{a}:{seq}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub fn watch(ctx: &Ctx<'_>, args: &WatchArgs) -> Result<()> {
    let kinds = wanted_kinds(args)?;
    let workspace = ctx.workspace()?;

    // Where to start. `--cursor` resumes, `--since` replays, and neither means
    // "from here on" — a watcher started now should not be handed a mailbox's
    // worth of history it has no way to tell apart from what just happened.
    let mut cursor = Cursor::default();
    let resume = match &ctx.global.cursor {
        Some(c) => Some(Cursor::parse(c)?),
        None => None,
    };
    let since = match &args.since {
        Some(s) => Some(parse_since(s, &ctx.clock)?),
        None => None,
    };
    for session in workspace.sessions() {
        let account = session.account_id.clone();
        let seq = match (&resume, since) {
            (Some(c), _) => c.get(&account).unwrap_or(0),
            (None, Some(at)) => session.store().change_seq_at(at)?,
            (None, None) => session.store().latest_change()?,
        };
        cursor.set(&account, seq);
    }

    // §6.4 promises `--json` is exactly one document, which a stream can never
    // be. Asking for one is therefore a request for a bounded read: everything
    // outstanding, and out.
    let drain = args.drain || (ctx.global.json && args.max_events.is_none());

    let syncing = if drain || args.no_sync {
        None
    } else {
        start_sync_engine(ctx, args, &workspace)
    };

    if ctx.out.is_structured() && !drain {
        ctx.out.ndjson_event(
            "watch",
            &serde_json::json!({
                "event": "started",
                "cursor": cursor.encode(),
                "accounts": mailbox_core::workspace::account_ids(&workspace),
                "syncing": syncing.is_some(),
            }),
        )?;
    } else if !ctx.out.is_structured() && !drain {
        ctx.out.print(format!(
            "watching {}{} — Ctrl-C to stop",
            mailbox_core::workspace::account_ids(&workspace).join(", "),
            match syncing {
                Some(_) => "",
                None => " (following; another process is syncing)",
            }
        ));
    }

    stream(ctx, args, &workspace, &mut cursor, &kinds, drain)
}

/// Reads the feed until there is nothing left to read, or forever.
fn stream(
    ctx: &Ctx<'_>,
    args: &WatchArgs,
    workspace: &Workspace,
    cursor: &mut Cursor,
    kinds: &[ChangeKind],
    drain: bool,
) -> Result<()> {
    let interval = Duration::from_millis(args.interval.max(10));
    let deadline = args
        .stop_after
        .map(|secs| Instant::now() + Duration::from_secs_f64(secs.max(0.0)));
    let mut emitted = 0usize;
    let mut collected: Vec<mailbox_api::Change> = Vec::new();
    let mut truncated = false;

    loop {
        let mut quiet = true;
        for session in workspace.sessions() {
            let account = &session.account_id;
            let after = cursor.get(account).unwrap_or(0);
            let page = session.store().changes_since(after, 500)?;
            truncated |= page.truncated;
            cursor.set(account, page.cursor);

            for change in page.changes {
                if !kinds.is_empty()
                    && !ChangeKind::parse(&change.event).is_some_and(|k| kinds.contains(&k))
                {
                    continue;
                }
                quiet = false;
                emitted += 1;
                if ctx.global.json {
                    collected.push(change);
                } else if ctx.out.is_structured() {
                    ctx.out.ndjson_event("watch", &change)?;
                } else {
                    print_change(ctx, &change);
                }
                if args.max_events.is_some_and(|max| emitted >= max) {
                    return finish(ctx, cursor, collected, truncated);
                }
            }
        }

        if drain {
            return finish(ctx, cursor, collected, truncated);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return finish(ctx, cursor, collected, truncated);
        }
        if quiet {
            // Only when nothing came back: a burst — a sync pass landing fifty
            // messages — is drained in one go rather than fifty intervals.
            std::thread::sleep(interval);
        }
    }
}

fn finish(
    ctx: &Ctx<'_>,
    cursor: &Cursor,
    changes: Vec<mailbox_api::Change>,
    truncated: bool,
) -> Result<()> {
    if ctx.global.json {
        ctx.out.reply(
            "watch",
            &mailbox_api::ChangeList {
                changes,
                cursor: cursor.encode(),
                truncated,
            },
        )?;
    } else if truncated {
        // Not a warning an agent can ignore: it means events happened that
        // this cursor will never see, so the mailbox has to be re-read.
        ctx.out.warn(
            "the feed was trimmed past this cursor — some events were missed; re-list to resync",
        );
    }
    Ok(())
}

fn print_change(ctx: &Ctx<'_>, change: &mailbox_api::Change) {
    let detail = match change.event.as_str() {
        "new" => text(&change.detail, "subject"),
        "flag" => change
            .detail
            .get("flags")
            .and_then(|f| f.as_array())
            .map(|f| {
                f.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "move" => match text(&change.detail, "from").as_str() {
            "" => String::new(),
            from => format!("from {from}"),
        },
        _ => String::new(),
    };
    let id = change.message.as_deref().unwrap_or_default();
    ctx.out.print(format!(
        "{:<8} {} {} {}",
        change.event,
        ctx.out.dim(&id[..id.len().min(7)]),
        change.folder.as_deref().unwrap_or_default(),
        ctx.out.paint(&detail, DIM),
    ));
}

fn text(detail: &serde_json::Value, key: &str) -> String {
    detail
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Which events to emit. Empty means all of them.
fn wanted_kinds(args: &WatchArgs) -> Result<Vec<ChangeKind>> {
    let Some(names) = &args.events else {
        return Ok(Vec::new());
    };
    let mut kinds = Vec::new();
    for name in names {
        let kind = ChangeKind::parse(name.trim()).ok_or_else(|| {
            // §6.2's rule, applied to a flag: an unknown name is an error, not
            // a filter that silently matches nothing.
            Error::usage(format!(
                "`{name}` is not an event; known events are {}",
                ChangeKind::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        kinds.push(kind);
    }
    Ok(kinds)
}

/// `--since 2h`, `--since 2026-09-01`: the query grammar's dates, read
/// backwards.
fn parse_since(input: &str, clock: &mailbox_core::Clock) -> Result<i64> {
    let s = input.trim();
    mailbox_core::query::parse::parse_date(s, clock)
        .or_else(|| mailbox_core::query::parse::parse_date(&format!("-{s}"), clock))
        .ok_or_else(|| {
            Error::usage(format!(
                "`--since {input}` is not a time; try `2h`, `7d`, or `2026-09-01`"
            ))
        })
}

/// Becomes the sync engine, in a thread, if nobody else is (§4.1).
///
/// Its own workspace, because a `Store` belongs to the thread that opened it
/// and because the tailing loop must keep reading while this one sits in
/// `IDLE` for nine minutes. Failing to own the accounts is not an error: it
/// means something else — the TUI, a `sync --watch` — is already keeping the
/// mailbox current, which is exactly the state this would have created.
fn start_sync_engine(ctx: &Ctx<'_>, args: &WatchArgs, workspace: &Workspace) -> Option<Stopper> {
    // Taken here, on this thread, so "am I the engine" is answered before the
    // first event rather than a round trip later. The locks move into the
    // thread and are held for as long as it runs.
    let mut owners = Vec::new();
    for session in workspace.sessions() {
        match mailbox_core::owner::Owner::try_acquire(session.store().dir(), &session.account_id) {
            Ok(Some(owner)) => owners.push(owner),
            // Partial ownership would mean syncing some accounts and not
            // others, which is a stranger thing to explain than not syncing.
            _ => return None,
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let opts = ctx.opts.clone();
    let tracer = ctx.tracer.clone();
    let clock = ctx.clock.clone();
    let idle_window = Duration::from_secs(args.idle_window.clamp(1, 29 * 60));
    let poll_interval = Duration::from_secs(args.poll.max(1));
    let flag = stop.clone();

    std::thread::Builder::new()
        .name("mailbox-sync".into())
        .spawn(move || {
            // The locks live here: dropped when this thread ends, which is
            // when the process does.
            let _owners = owners;
            let mut engine = match Workspace::open(&opts, tracer, clock) {
                Ok(w) => w,
                // Nothing to report to: the stream is the output, and a
                // watcher that cannot sync still follows correctly.
                Err(_) => return,
            };
            let options = mailbox_core::imap::watch::WatchOptions {
                sync: mailbox_core::imap::sync::SyncOptions::default(),
                idle_window,
                poll_interval,
                max_passes: None,
                ..Default::default()
            };
            let _ = mailbox_core::imap::watch::watch(&mut engine, &options, |_, _, _| {
                !flag.load(Ordering::Relaxed)
            });
        })
        .ok()?;

    Some(Stopper(stop))
}

/// Tells the sync thread to stop at its next wake. Dropping it is enough.
struct Stopper(Arc<AtomicBool>);

impl Drop for Stopper {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

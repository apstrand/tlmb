//! `--trace` (§6.7): an NDJSON record of every IMAP exchange, every store
//! transaction, and every state-machine transition, each with a monotonic
//! sequence number.
//!
//! This ships in release builds rather than behind `#[cfg(test)]`, because the
//! same stream that lets a test assert on an event sequence — instead of on
//! wall-clock behaviour — is what makes a user's sync bug report actionable.
//! It is inert until a path is set: [`Tracer::disabled`] costs an `Option`
//! check per call site and nothing else.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::clock::Clock;

/// The event names the engine emits. They are a public interface: `sync --once
/// --until <event>` takes one, and tests assert on them, so renaming one is a
/// breaking change to the same degree as renaming a JSON field.
pub mod event {
    pub const IMAP_CONNECT: &str = "imap.connect";
    pub const IMAP_SEND: &str = "imap.send";
    pub const IMAP_RECV: &str = "imap.recv";
    pub const IMAP_DISCONNECT: &str = "imap.disconnect";
    pub const REPLAY_BEGIN: &str = "replay.begin";
    pub const REPLAY_OP: &str = "replay.op";
    pub const REPLAY_END: &str = "replay.end";
    pub const SYNC_BEGIN: &str = "sync.begin";
    pub const SYNC_FOLDER_BEGIN: &str = "sync.folder.begin";
    pub const SYNC_FOLDER_END: &str = "sync.folder.end";
    pub const SYNC_UIDVALIDITY_RESET: &str = "sync.uidvalidity_reset";
    pub const SYNC_MESSAGES_APPLIED: &str = "sync.messages_applied";
    pub const SYNC_BODIES_FETCHED: &str = "sync.bodies_fetched";
    pub const SYNC_END: &str = "sync.end";
    pub const STORE_TXN_BEGIN: &str = "store.txn.begin";
    pub const STORE_TXN_COMMIT: &str = "store.txn.commit";
    pub const STORE_TXN_ROLLBACK: &str = "store.txn.rollback";
    pub const STORE_MIGRATE: &str = "store.migrate";
    pub const THREAD_REBUILD: &str = "thread.rebuild";
}

struct Sink {
    out: BufWriter<Box<dyn Write + Send>>,
}

/// A handle to the trace stream. Cloning shares the sink and the counter, so
/// sequence numbers stay monotonic across threads.
#[derive(Clone)]
pub struct Tracer {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    sink: Mutex<Sink>,
    seq: AtomicU64,
    clock: Clock,
    /// `--trace-bodies`. Off by default: a trace a user can attach to a bug
    /// report must not carry their mail.
    bodies: bool,
    /// `sync --once --until <event>`.
    until: Option<String>,
    reached: AtomicBool,
}

impl Tracer {
    /// The normal case: every call becomes a branch that does nothing.
    pub fn disabled() -> Tracer {
        Tracer { inner: None }
    }

    pub fn to_file(
        path: &std::path::Path,
        clock: Clock,
        bodies: bool,
        until: Option<String>,
    ) -> std::io::Result<Tracer> {
        let file = File::create(path)?;
        Ok(Tracer::to_writer(Box::new(file), clock, bodies, until))
    }

    /// `until` arms `sync --once --until <event>`: the run stops at the next
    /// clean boundary after that event is emitted.
    pub fn to_writer(
        w: Box<dyn Write + Send>,
        clock: Clock,
        bodies: bool,
        until: Option<String>,
    ) -> Tracer {
        Tracer {
            inner: Some(Arc::new(Inner {
                sink: Mutex::new(Sink {
                    out: BufWriter::new(w),
                }),
                seq: AtomicU64::new(0),
                clock,
                bodies,
                until,
                reached: AtomicBool::new(false),
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// True once `--until`'s event has been emitted. The sync engine checks
    /// this between units of work and stops cleanly at the next boundary.
    pub fn stop_requested(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|i| i.reached.load(Ordering::SeqCst))
    }

    /// Whether message bodies may appear in the stream (`--trace-bodies`).
    pub fn bodies(&self) -> bool {
        self.inner.as_ref().is_some_and(|i| i.bodies)
    }

    /// Emits one event. `data` is any JSON value; `null` for events whose name
    /// says everything.
    pub fn emit(&self, kind: &str, data: serde_json::Value) {
        let Some(inner) = &self.inner else { return };
        let seq = inner.seq.fetch_add(1, Ordering::SeqCst);
        let record = serde_json::json!({
            "seq": seq,
            "t": crate::clock::format_rfc3339(inner.clock.now()),
            "event": kind,
            "data": data,
        });
        if let Ok(mut sink) = inner.sink.lock() {
            let _ = writeln!(sink.out, "{record}");
            // Flushing every event is deliberate: a trace whose last thousand
            // events were lost to a buffer is worthless for the crash-only
            // oracle (§14.3), which reads it after a SIGKILL.
            let _ = sink.out.flush();
        }
        if inner.until.as_deref() == Some(kind) {
            inner.reached.store(true, Ordering::SeqCst);
        }
    }

    /// Convenience for the many events whose payload is one key.
    pub fn emit1(&self, kind: &str, key: &str, value: impl serde::Serialize) {
        if self.inner.is_none() {
            return;
        }
        self.emit(kind, serde_json::json!({ key: serde_json::to_value(value).unwrap_or(serde_json::Value::Null) }));
    }
}

impl std::fmt::Debug for Tracer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracer")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl Default for Tracer {
    fn default() -> Self {
        Tracer::disabled()
    }
}

/// Strips credentials from an IMAP command before it reaches the trace.
///
/// `LOGIN`, `AUTHENTICATE`, and the base64 continuation that follows an
/// `AUTHENTICATE` all carry secrets. Redaction is by command name rather than
/// by pattern-matching the payload, because a missed pattern writes a password
/// to a file the user is about to attach to a bug report.
pub fn redact_command(line: &str) -> String {
    let rest = line.split_once(' ').map(|(_, r)| r).unwrap_or(line);
    let verb = rest.split(' ').next().unwrap_or("").to_ascii_uppercase();
    match verb.as_str() {
        "LOGIN" => {
            let tag = line.split(' ').next().unwrap_or("*");
            let user = rest.split(' ').nth(1).unwrap_or("");
            format!("{tag} LOGIN {user} <redacted>")
        }
        "AUTHENTICATE" => {
            let tag = line.split(' ').next().unwrap_or("*");
            let mech = rest.split(' ').nth(1).unwrap_or("");
            format!("{tag} AUTHENTICATE {mech} <redacted>")
        }
        _ => line.to_string(),
    }
}

/// Truncates a server response for the trace. Full literals are the bulk of a
/// FETCH and are almost never what a reader needs.
pub fn summarize_response(line: &str, limit: usize) -> String {
    if line.len() <= limit {
        return line.to_string();
    }
    let cut = line
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|i| *i <= limit)
        .last()
        .unwrap_or(0);
    format!("{}… <{} bytes total>", &line[..cut], line.len())
}

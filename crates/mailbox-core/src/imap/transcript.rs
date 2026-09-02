//! **L1 — recorded-transcript replay** (§14.2), VCR-style.
//!
//! A transcript is the exact bytes of one IMAP conversation. Recording one costs
//! a real server; replaying it costs nothing, runs in milliseconds, and needs no
//! network at all — which is what makes it worth gating every PR on.
//!
//! Two rules from §14.2 shape the format:
//!
//! - **A stale transcript is a test that passes about a server that no longer
//!   exists.** So every transcript carries the date it was recorded and what the
//!   server said it was, and [`Transcript::age_days`] lets a suite refuse one
//!   that has gone stale rather than quietly trusting it.
//! - **Replay asserts on what the client sent**, not just on what it read. A
//!   recording that only replayed responses would let the engine send anything
//!   at all and still pass; the point is that the conversation is the same one.
//!
//! Credentials never reach a transcript: the `LOGIN`/`AUTHENTICATE` exchange is
//! redacted on the way in, exactly as it is for `--trace` (§6.7), because a
//! transcript is a file that gets committed to a repository.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One side of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "dir", rename_all = "snake_case")]
pub enum Event {
    /// Bytes the client wrote.
    Sent {
        #[serde(with = "b64")]
        bytes: Vec<u8>,
    },
    /// Bytes the server wrote.
    Received {
        #[serde(with = "b64")]
        bytes: Vec<u8>,
    },
}

impl Event {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Event::Sent { bytes } | Event::Received { bytes } => bytes,
        }
    }
}

/// A recorded conversation, plus what it is a recording *of*.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    /// RFC3339. Read by [`Transcript::age_days`].
    pub recorded_at: String,
    /// What the server called itself — its greeting line, trimmed. A
    /// transcript from Dovecot 2.3 and one from Cyrus are different tests, and
    /// the file should say which it is.
    pub server: String,
    /// A one-line description of what the conversation does.
    #[serde(default)]
    pub scenario: String,
    /// What the server advertised after authentication.
    ///
    /// Recorded because a transcript begins *after* login: the capabilities
    /// decide which of §5.3's paths the engine takes, so replaying without them
    /// would test a different engine than the one that was recorded.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Whether `QRESYNC` was enabled on the recorded connection.
    #[serde(default)]
    pub qresync: bool,
    /// The command counter the recorded connection had reached.
    ///
    /// A transcript starts mid-session, so its first command is not `a0001`.
    /// Replaying from 1 would diverge on the tag alone — which would be a real
    /// divergence, just not an interesting one.
    #[serde(default)]
    pub start_tag: u64,
    /// The sync options the recorded pass ran with.
    ///
    /// A recording is of a pass that asked particular questions; replaying with
    /// different options would ask for bytes the recording does not contain and
    /// diverge for a reason that is about the request, not the engine.
    #[serde(default)]
    pub sync_options: Option<crate::imap::sync::SyncOptions>,
    pub events: Vec<Event>,
}

impl Transcript {
    /// Stored as NDJSON: one header line, then one line per event.
    ///
    /// Line-oriented so a diff of a re-recorded transcript is readable — a
    /// single-JSON-blob format would show every refresh as one changed line.
    pub fn load(path: &Path) -> Result<Transcript> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::usage(format!("{}: {e}", path.display())))?;
        let mut lines = text.lines();
        let header = lines
            .next()
            .ok_or_else(|| Error::usage(format!("{}: empty transcript", path.display())))?;
        let mut transcript: Transcript = serde_json::from_str(header)
            .map_err(|e| Error::usage(format!("{}: bad header: {e}", path.display())))?;
        transcript.events.clear();
        for (i, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            transcript.events.push(
                serde_json::from_str(line).map_err(|e| {
                    Error::usage(format!("{}: line {}: {e}", path.display(), i + 2))
                })?,
            );
        }
        Ok(transcript)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        let header = Transcript {
            events: Vec::new(),
            ..self.clone()
        };
        out.push_str(&serde_json::to_string(&header)?);
        out.push('\n');
        for event in &self.events {
            out.push_str(&serde_json::to_string(event)?);
            out.push('\n');
        }
        std::fs::write(path, out)?;
        Ok(())
    }

    /// How old the recording is, for a suite that refuses stale ones.
    pub fn age_days(&self, now: crate::clock::Timestamp) -> i64 {
        let recorded = crate::clock::parse_timestamp(&self.recorded_at).unwrap_or(now);
        (now - recorded).max(0) / 86_400
    }

    /// How many commands the client issued — a cheap sanity check that a
    /// recording captured a whole conversation rather than a connection error.
    pub fn commands(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, Event::Sent { .. }))
            .count()
    }
}

/// Tees a live connection into a transcript.
///
/// Held behind a mutex because the connection is read and written from the same
/// thread but the handle is shared with whoever will save the result.
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<Transcript>>,
    /// Recording begins after authentication, so a transcript is the sync
    /// conversation and nothing else.
    ///
    /// Two reasons. A credential must never reach a file that gets committed,
    /// and the surest way to guarantee that is for the exchange carrying it to
    /// be outside the recording. And a replay has no credential to offer, so a
    /// transcript that began with `AUTHENTICATE` could never be replayed
    /// faithfully anyway.
    armed: Arc<Mutex<bool>>,
}

impl Recorder {
    pub fn new(server: &str, scenario: &str, now: crate::clock::Timestamp) -> Recorder {
        Recorder {
            inner: Arc::new(Mutex::new(Transcript {
                recorded_at: crate::clock::format_rfc3339(now),
                server: server.to_string(),
                scenario: scenario.to_string(),
                capabilities: Vec::new(),
                qresync: false,
                start_tag: 0,
                sync_options: None,
                events: Vec::new(),
            })),
            armed: Arc::new(Mutex::new(false)),
        }
    }

    /// Starts capturing, and records the state the connection reached.
    pub fn start(&self, capabilities: Vec<String>, qresync: bool, tag: u64) {
        if let Ok(mut t) = self.inner.lock() {
            t.capabilities = capabilities;
            t.qresync = qresync;
            t.start_tag = tag;
            t.events.clear();
        }
        *self.armed.lock().expect("armed lock") = true;
    }

    /// Notes the options the pass being recorded is running with.
    pub fn set_sync_options(&self, opts: &crate::imap::sync::SyncOptions) {
        if let Ok(mut t) = self.inner.lock() {
            t.sync_options = Some(opts.clone());
        }
    }

    fn is_armed(&self) -> bool {
        *self.armed.lock().expect("armed lock")
    }

    /// Records bytes the client sent, redacting any credential in them.
    pub fn sent(&self, bytes: &[u8]) {
        let redacted = redact_sent(bytes);
        self.push(Event::Sent { bytes: redacted });
    }

    pub fn received(&self, bytes: &[u8]) {
        self.push(Event::Received {
            bytes: bytes.to_vec(),
        });
    }

    fn push(&self, event: Event) {
        if !self.is_armed() {
            return;
        }
        if let Ok(mut t) = self.inner.lock() {
            // Consecutive events in the same direction are one exchange as far
            // as replay cares, and merging them keeps a transcript readable.
            match (t.events.last_mut(), &event) {
                (Some(Event::Sent { bytes: prev }), Event::Sent { bytes }) => {
                    prev.extend_from_slice(bytes)
                }
                (Some(Event::Received { bytes: prev }), Event::Received { bytes }) => {
                    prev.extend_from_slice(bytes)
                }
                _ => t.events.push(event),
            }
        }
    }

    /// Notes what the server called itself, from its greeting.
    pub fn set_server(&self, greeting: &str) {
        if let Ok(mut t) = self.inner.lock() {
            if t.server.is_empty() {
                t.server = greeting.trim().to_string();
            }
        }
    }

    pub fn finish(&self) -> Transcript {
        self.inner.lock().expect("transcript lock").clone()
    }
}

/// Replaces the secret in a `LOGIN` or `AUTHENTICATE` exchange.
///
/// A transcript is a file that gets committed, so this is not optional. The
/// replacement keeps the byte structure — same command, same line ending — so a
/// replayed run still exercises the same parsing.
fn redact_sent(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive("\r\n").enumerate() {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let upper = trimmed.to_ascii_uppercase();
        // The base64 blob after `AUTHENTICATE` is a bare line with no verb, so
        // it is caught by position: it is the line right after the command.
        let is_sasl_payload = i > 0
            && !trimmed.is_empty()
            && !trimmed.contains(' ')
            && trimmed.len() > 8
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c));

        if upper.contains(" LOGIN ") || upper.contains(" AUTHENTICATE ") || is_sasl_payload {
            out.push_str(&crate::trace::redact_command(trimmed));
            out.push_str("\r\n");
        } else {
            out.push_str(line);
        }
    }
    out.into_bytes()
}

/// The other half: a transport that serves a recording back.
pub struct Replay {
    events: VecDeque<Event>,
    /// Bytes the client has written that have not yet matched an expected
    /// `Sent` event.
    pending_write: Vec<u8>,
    /// Bytes not yet handed to the client.
    pending_read: VecDeque<u8>,
    /// Set on the first divergence. Shared so a test can read it after the run
    /// has failed for some downstream reason.
    divergence: Arc<Mutex<Option<String>>>,
    /// Whether writes are checked at all.
    strict: bool,
}

impl Replay {
    pub fn new(transcript: Transcript) -> Replay {
        Replay {
            events: transcript.events.into(),
            pending_write: Vec::new(),
            pending_read: VecDeque::new(),
            divergence: Arc::new(Mutex::new(None)),
            strict: true,
        }
    }

    /// Stops checking what the client sends.
    ///
    /// For a transcript replayed against a deliberately different scenario,
    /// where the commands are *expected* to diverge and only the parsing is
    /// under test.
    pub fn lenient(mut self) -> Replay {
        self.strict = false;
        self
    }

    /// A handle to whatever went wrong, readable after the run.
    pub fn divergence(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.divergence)
    }

    fn diverge(&mut self, what: String) {
        let mut slot = self.divergence.lock().expect("divergence lock");
        if slot.is_none() {
            *slot = Some(what);
        }
    }

    /// Consumes any `Sent` events the client's writes now satisfy.
    fn settle_writes(&mut self) {
        while let Some(Event::Sent { bytes: expected }) = self.events.front() {
            if self.pending_write.len() < expected.len() {
                // Still mid-command; wait for the rest.
                return;
            }
            let (head, rest) = self.pending_write.split_at(expected.len());
            if self.strict && head != expected.as_slice() {
                let what = format!(
                    "the client sent {:?} where the recording has {:?}",
                    String::from_utf8_lossy(&head[..head.len().min(200)]),
                    String::from_utf8_lossy(&expected[..expected.len().min(200)])
                );
                self.diverge(what);
                // Drop the expectation so the run can continue to its own
                // failure rather than deadlocking here.
                self.events.pop_front();
                self.pending_write.clear();
                return;
            }
            self.pending_write = rest.to_vec();
            self.events.pop_front();
        }
    }
}

impl Write for Replay {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending_write.extend_from_slice(buf);
        self.settle_writes();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for Replay {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pending_read.is_empty() {
            self.settle_writes();
            match self.events.pop_front() {
                Some(Event::Received { bytes }) => self.pending_read.extend(bytes),
                Some(Event::Sent { bytes }) => {
                    // The recording expected the client to say something and it
                    // read instead. Reported rather than hidden, because a
                    // client that stops talking mid-conversation is exactly the
                    // regression this level catches.
                    self.diverge(format!(
                        "the client read where the recording expected it to send {:?}",
                        String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
                    ));
                    return Ok(0);
                }
                // The recording ran out: the server would have closed.
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending_read.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.pending_read.pop_front().expect("checked len");
        }
        Ok(n)
    }
}

/// base64 for the byte fields, so a transcript is one line per event and safe
/// to put in a repository whatever the message content is.
mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

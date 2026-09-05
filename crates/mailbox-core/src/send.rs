//! Sending, at the session level: build, check, enqueue, and flush (§5.5, §6.5).
//!
//! The order is the guarantee. A message is built and policy-checked *before*
//! anything is written, so a refusal leaves no trace; it is then written to the
//! outbox in one transaction; and only then does anything reach the network.
//! Nothing can be half-sent because nothing is sent until it is durable.

use mailbox_api::{OutboxItem, SendResult};

use crate::clock::format_rfc3339;
use crate::compose::{self, Draft};
use crate::error::{Error, Result};
use crate::policy::{Policy, SendContext};
use crate::session::{IdRef, Session};
use crate::store::outbox::{NewOutboxEntry, OutboxState};
use crate::trace::event;

/// How a caller asked for a message to go out.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    pub draft: Draft,
    /// §6.5. A repeat with the same key returns the original result.
    pub idempotency_key: Option<String>,
    /// The undo-send window in seconds (§5.5). Zero for the CLI unless
    /// `--delay` is given.
    pub delay_seconds: u64,
    /// Render the bytes and stop (§6.5).
    pub dry_run: bool,
    /// `--yes`, which satisfies the policy's external-recipient confirmation.
    pub confirmed: bool,
    /// Hold as a draft rather than queueing it.
    pub draft_only: bool,
}

/// How `reply` should address the new message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyMode {
    /// Just the sender.
    Sender,
    /// Everyone: `--all`.
    All,
    /// `--forward`: nobody, and the body is quoted differently.
    Forward,
}

impl Session {
    /// Builds, checks, and enqueues a message.
    ///
    /// Never sends: the outbox is flushed by [`Session::flush_outbox`] or by a
    /// sync pass. Separating them is what makes the undo-send window (§5.5) and
    /// the approval queue (§6.5) possible at all, and what makes a crash
    /// between "queued" and "sent" recoverable rather than ambiguous.
    pub fn enqueue_send(&mut self, req: &SendRequest) -> Result<SendResult> {
        let account = self.account_config()?.clone();

        // Fill in the sender before anything looks at the draft: policy and
        // `Message-ID` generation both depend on it.
        let mut draft = req.draft.clone();
        if draft.from.is_none() {
            draft.from = Some(mailbox_api::Address::new(None, account.address.clone()));
        }
        let from_address = draft
            .from
            .as_ref()
            .map(|a| a.address.to_ascii_lowercase())
            .unwrap_or_default();

        // §6.5: the policy runs before anything is written, so a denied send
        // leaves nothing behind to clean up.
        let policy = Policy::load_default()?;
        let ctx = SendContext {
            sends_last_hour: self.store().sends_since(self.clock.now() - 3600)?,
            confirmed: req.confirmed,
        };
        policy.check(&draft, &ctx).map_err(Error::from)?;

        let bytes = compose::build(
            &draft,
            &self.clock,
            &self.rng,
            &compose::domain_for(&from_address),
        )?;
        let recipients = draft.envelope_recipients();

        if req.dry_run {
            // §6.5: renders the exact bytes and exits without touching the
            // network — or, here, the store.
            return Ok(SendResult {
                id: 0,
                message_id: String::new(),
                status: "dry_run".into(),
                recipients,
                bytes: bytes.len() as u64,
                send_after: None,
                duplicate: false,
                raw: Some(String::from_utf8_lossy(&bytes).into_owned()),
            });
        }

        // §6.5's key check happens before the blob is written, so a repeat
        // costs nothing.
        if let Some(key) = &req.idempotency_key {
            if let Some(existing) = self.store().outbox_by_key(key, self.clock.now())? {
                let mut result: SendResult = existing
                    .result
                    .as_deref()
                    .and_then(|r| serde_json::from_str(r).ok())
                    .unwrap_or(SendResult {
                        id: existing.id,
                        message_id: existing.message_id.clone(),
                        status: existing.state.as_str().to_string(),
                        recipients: existing.envelope_to.clone(),
                        bytes: 0,
                        send_after: None,
                        duplicate: false,
                        raw: None,
                    });
                result.duplicate = true;
                return Ok(result);
            }
        }

        let parsed = crate::mime::parse(&bytes);
        let message_id = crate::id::message_id(
            &self.account_id,
            parsed.message_id.as_deref(),
            &crate::id::Fallback {
                date: self.clock.now(),
                from: &from_address,
                subject: &draft.subject,
                size: bytes.len() as u64,
            },
        );
        let blob_id = self.store().blobs().put(&bytes)?;

        // §6.5: `approve` is the default under MAILBOX_AGENT=1, whatever the
        // config says.
        let state = if req.draft_only {
            OutboxState::Draft
        } else if self.config.send_policy(self.agent_mode) == "approve" {
            OutboxState::PendingApproval
        } else {
            OutboxState::Queued
        };
        let send_after = self.clock.now() + req.delay_seconds as i64;

        let (id, created) = self.store_mut().enqueue_outbox(&NewOutboxEntry {
            message_id: message_id.clone(),
            blob_id,
            envelope_from: from_address,
            envelope_to: recipients.clone(),
            state,
            idempotency_key: req.idempotency_key.clone(),
            send_after,
        })?;

        self.tracer.emit(
            event::OUTBOX_ENQUEUE,
            serde_json::json!({
                "id": id,
                "state": state.as_str(),
                "recipients": recipients.len(),
                "bytes": bytes.len(),
            }),
        );

        let result = SendResult {
            id,
            message_id,
            status: state.as_str().to_string(),
            recipients,
            bytes: bytes.len() as u64,
            send_after: (req.delay_seconds > 0).then(|| format_rfc3339(send_after)),
            duplicate: !created,
            raw: None,
        };
        // Recorded so a repeat of the key returns this document verbatim rather
        // than a reconstruction of it.
        if req.idempotency_key.is_some() {
            let json = serde_json::to_string(&result)?;
            self.store_mut().set_outbox_result(id, &json)?;
        }
        Ok(result)
    }

    /// Sends everything the outbox has ready, and files the `Sent` copies.
    pub fn flush_outbox(&mut self) -> Result<mailbox_api::SubmitReport> {
        let account = self.account_config()?.clone();
        let host = account.smtp_host.clone().ok_or_else(|| {
            Error::usage(format!(
                "account `{}` has no smtp_host; autoconfig discovery lands with `accounts add`",
                self.account_id
            ))
        })?;
        let credential = crate::credentials::resolve(&self.account_id, &account)?;

        let cfg = crate::smtp::SmtpConfig {
            host: host.clone(),
            port: account.smtp_port(),
            security: account.smtp_security(),
            require_tls: account.require_tls || !crate::session::is_loopback(&host),
            timeout: std::time::Duration::from_millis(self.timeout_ms()),
            // §2: the server is told nothing it did not need. The sender's
            // domain is already in the envelope; the local host name is not.
            ehlo_name: compose::domain_for(&account.address),
        };

        let report = crate::smtp::flush(
            self.store_mut(),
            &cfg,
            account.smtp_login_user(),
            &credential,
        )?;

        // Filing the Sent copies needs IMAP, and is worth its own connection
        // only when there is something to file.
        let mut uploaded = 0;
        if !self.store().outbox_awaiting_upload()?.is_empty() {
            if let Ok(mut client) = self.connect() {
                uploaded = crate::smtp::upload_sent(&mut client, self.store_mut())?;
                let _ = client.logout();
            }
        }

        Ok(mailbox_api::SubmitReport {
            attempted: report.attempted,
            sent: report.sent,
            deferred: report.deferred,
            failed: report.failed,
            uploaded,
        })
    }

    /// Builds a reply, a reply-all, or a forward from an existing message (§6.1).
    pub fn reply_draft(&self, id_ref: &IdRef, mode: ReplyMode) -> Result<Draft> {
        let id = self.resolve(id_ref)?;
        let original = self.store().message(&id, &Default::default())?;
        let identities = self.account_config()?.all_identities();

        let mut draft = Draft {
            subject: reply_subject(&original.summary.subject, mode),
            // §7: the From selector defaults to the identity the thread was
            // addressed to. Answering from the address they wrote to is what
            // anyone would do by hand.
            from: Some(choose_identity(&original, &identities)),
            ..Default::default()
        };

        match mode {
            ReplyMode::Forward => {}
            ReplyMode::Sender | ReplyMode::All => {
                // Reply-To wins over From when the sender asked for it.
                draft.to = if original.reply_to.is_empty() {
                    original.summary.from.clone()
                } else {
                    original.reply_to.clone()
                };
                if mode == ReplyMode::All {
                    for a in original.summary.to.iter().chain(&original.summary.cc) {
                        let lower = a.address.to_ascii_lowercase();
                        // Never reply to yourself, and never twice to one person.
                        if identities.contains(&lower)
                            || draft
                                .to
                                .iter()
                                .any(|x| x.address.eq_ignore_ascii_case(&a.address))
                            || draft
                                .cc
                                .iter()
                                .any(|x| x.address.eq_ignore_ascii_case(&a.address))
                        {
                            continue;
                        }
                        draft.cc.push(a.clone());
                    }
                }

                // Threading (§5.6) depends on these being right: In-Reply-To is
                // the parent, References is the ancestry plus the parent.
                if let Some(mid) = &original.message_id {
                    draft.in_reply_to = Some(format!("<{}>", mid.trim_matches(['<', '>'])));
                    let mut refs: Vec<String> = original
                        .references
                        .iter()
                        .map(|r| format!("<{}>", r.trim_matches(['<', '>'])))
                        .collect();
                    let parent = format!("<{}>", mid.trim_matches(['<', '>']));
                    if !refs.contains(&parent) {
                        refs.push(parent);
                    }
                    draft.references = refs;
                }
            }
        }

        draft.body = quoted_body(&original, mode);
        Ok(draft)
    }
}

/// `Re:` once, `Fwd:` once — never `Re: Re: Re:`.
/// [`reply_subject`], reachable from the test suite.
pub fn reply_subject_for_test(subject: &str, mode: ReplyMode) -> String {
    reply_subject(subject, mode)
}

fn reply_subject(subject: &str, mode: ReplyMode) -> String {
    let stripped = crate::threading::normalize_subject(subject);
    let base = if stripped.is_empty() {
        subject.trim().to_string()
    } else {
        // Recover the original casing: the normalized form is folded, and
        // shouting a reply subject back at someone in lowercase is rude.
        original_case(subject, &stripped)
    };
    match mode {
        ReplyMode::Forward => format!("Fwd: {base}"),
        _ => format!("Re: {base}"),
    }
}

/// Finds the tail of `subject` that the normalizer kept, so prefixes are
/// stripped without folding the case of what remains.
fn original_case(subject: &str, normalized: &str) -> String {
    let trimmed = subject.trim();
    // The *shortest* suffix that still normalizes the same way. Taking the
    // first match instead of the shortest returns the whole subject every
    // time — the full string normalizes to the normalized form by definition,
    // which is what normalizing means — so `Re: the parser` came back with its
    // prefix intact and the reply went out as `Re: Re: the parser`. Down a
    // long thread that compounds.
    let mut best = trimmed;
    let mut start = 0;
    while start < trimmed.len() {
        let candidate = &trimmed[start..];
        if crate::threading::normalize_subject(candidate) == normalized {
            best = candidate;
        }
        // Advance one character, staying on a boundary.
        start += trimmed[start..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    best.to_string()
}

/// The identity to answer from: whichever of ours the message was addressed to,
/// falling back to the account's primary address.
fn choose_identity(
    original: &mailbox_api::MessageDetail,
    identities: &[String],
) -> mailbox_api::Address {
    for a in original.summary.to.iter().chain(&original.summary.cc) {
        if identities.contains(&a.address.to_ascii_lowercase()) {
            return mailbox_api::Address::new(None, a.address.clone());
        }
    }
    mailbox_api::Address::new(None, identities.first().cloned().unwrap_or_default())
}

/// The quoted original, with the attribution line above it.
fn quoted_body(original: &mailbox_api::MessageDetail, mode: ReplyMode) -> String {
    let sender = original
        .summary
        .from
        .first()
        .map(|a| match &a.name {
            Some(n) => format!("{n} <{}>", a.address),
            None => a.address.clone(),
        })
        .unwrap_or_else(|| "someone".into());

    let body = original
        .body
        .as_ref()
        .map(|b| b.content.as_str())
        .unwrap_or("");

    match mode {
        ReplyMode::Forward => {
            let mut out = String::from("\n\n---------- Forwarded message ----------\n");
            out.push_str(&format!("From: {sender}\n"));
            out.push_str(&format!("Date: {}\n", original.summary.date));
            out.push_str(&format!("Subject: {}\n", original.summary.subject));
            if !original.summary.to.is_empty() {
                let to: Vec<String> = original
                    .summary
                    .to
                    .iter()
                    .map(|a| a.address.clone())
                    .collect();
                out.push_str(&format!("To: {}\n", to.join(", ")));
            }
            out.push('\n');
            out.push_str(body);
            out
        }
        _ => {
            let mut out = format!("\n\nOn {}, {sender} wrote:\n", original.summary.date);
            for line in body.lines() {
                out.push_str(if line.is_empty() { ">" } else { "> " });
                out.push_str(line);
                out.push('\n');
            }
            out
        }
    }
}

/// Renders an outbox entry for the contract.
pub fn outbox_item(store: &crate::store::Store, entry: &crate::store::OutboxEntry) -> OutboxItem {
    // Subject and sender come from the stored bytes rather than a column: they
    // are already there, and a second copy is a second thing to keep true.
    let parsed = store
        .blobs()
        .get(&entry.blob_id)
        .ok()
        .map(|b| crate::mime::parse(&b));
    OutboxItem {
        id: entry.id,
        message_id: entry.message_id.clone(),
        subject: parsed
            .as_ref()
            .map(|p| p.subject.clone())
            .unwrap_or_default(),
        from: entry.envelope_from.clone(),
        recipients: entry.envelope_to.clone(),
        state: entry.state.as_str().to_string(),
        attempts: entry.attempts,
        last_error: entry.last_error.clone(),
        created: format_rfc3339(entry.created),
        sent_at: entry.sent_at.map(format_rfc3339),
        filed: entry.sent_uploaded,
    }
}

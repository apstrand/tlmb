//! SMTP submission (§5.5). RFC 6409 submission, not relay — §2 is explicit that
//! this speaks IMAP and SMTP submission and nothing else on the wire.
//!
//! Written for the same reason as the IMAP client: §6.7 wants every command and
//! response in the trace with credentials redacted, and the fault matrix of L3
//! wants a client whose failure handling is ours to reason about.
//!
//! The one subtlety that shapes the whole module is the **double-send window**.
//! Between sending the final `.` and reading the `250`, the client cannot know
//! whether the server queued the message. §16 decides that: retry, and accept
//! that this can deliver twice. What the code owes that decision is honesty
//! about *which* failure happened, so a caller can tell "certainly not sent"
//! from "possibly sent" — hence [`SendOutcome`].

pub mod client;
pub mod submit;

pub use client::{SmtpClient, SmtpConfig};
pub use submit::{flush, upload_sent, SubmitReport};

/// How far a submission attempt got.
///
/// The distinction is the point. Everything up to and including the `DATA`
/// command is safely retryable: the server never saw a whole message. Once the
/// terminating `.` is on the wire, it might have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The server answered `250` after the body. Delivered, once.
    Accepted {
        /// The server's response text, which often carries a queue id worth
        /// keeping for a support conversation.
        response: String,
    },
    /// The server refused, permanently (5xx). Retrying will not help.
    Rejected { code: u16, message: String },
    /// The server refused, temporarily (4xx), or the connection failed before
    /// the message body was complete. Safe to retry: nothing was delivered.
    Deferred { message: String },
    /// The body was sent and the reply never arrived — the double-send window.
    ///
    /// Retrying may deliver twice. §16 accepts that, and this variant exists so
    /// the decision is visible in the code rather than buried in a `bool`.
    Uncertain { message: String },
}

impl SendOutcome {
    pub fn is_accepted(&self) -> bool {
        matches!(self, SendOutcome::Accepted { .. })
    }

    /// Whether the outbox should try again.
    ///
    /// `Uncertain` retries per §16 — a duplicate is a smaller harm than a
    /// silent loss.
    pub fn should_retry(&self) -> bool {
        matches!(
            self,
            SendOutcome::Deferred { .. } | SendOutcome::Uncertain { .. }
        )
    }

    pub fn message(&self) -> String {
        match self {
            SendOutcome::Accepted { response } => response.clone(),
            SendOutcome::Rejected { code, message } => format!("{code} {message}"),
            SendOutcome::Deferred { message } | SendOutcome::Uncertain { message } => {
                message.clone()
            }
        }
    }
}

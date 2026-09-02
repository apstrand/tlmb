//! The core's error type, and its mapping onto the wire contract (§6.4, §6.6).
//!
//! Every variant carries enough to produce a structured error without the
//! frontend having to re-classify a string — "never a bare English sentence"
//! is a property of the type, not of the caller's diligence.

use mailbox_api::{ErrorCode, ErrorResponse};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no such {kind}: {what}")]
    NotFound { kind: &'static str, what: String },

    #[error("{prefix} matches {count} messages")]
    Ambiguous { prefix: String, count: usize },

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("the server broke the protocol: {0}")]
    Protocol(String),

    #[error("local state disagrees with the server: {0}")]
    Conflict(String),

    #[error("store error: {0}")]
    Store(String),

    #[error("{0}")]
    Usage(String),

    #[error("timed out after {0} ms")]
    Timeout(u64),

    #[error("{message}")]
    NeedsConfirmation { message: String, hint: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn not_found(kind: &'static str, what: impl Into<String>) -> Error {
        Error::NotFound {
            kind,
            what: what.into(),
        }
    }

    pub fn usage(msg: impl Into<String>) -> Error {
        Error::Usage(msg.into())
    }

    pub fn store(msg: impl Into<String>) -> Error {
        Error::Store(msg.into())
    }

    pub fn protocol(msg: impl Into<String>) -> Error {
        Error::Protocol(msg.into())
    }

    pub fn code(&self) -> ErrorCode {
        match self {
            Error::NotFound { .. } => ErrorCode::NotFound,
            Error::Ambiguous { .. } => ErrorCode::Ambiguous,
            Error::Auth(_) => ErrorCode::AuthFailed,
            Error::Network(_) | Error::Io(_) => ErrorCode::Network,
            Error::Protocol(_) => ErrorCode::Protocol,
            Error::Conflict(_) => ErrorCode::Conflict,
            Error::Store(_) => ErrorCode::Store,
            Error::Usage(_) => ErrorCode::Usage,
            Error::Timeout(_) => ErrorCode::Timeout,
            Error::NeedsConfirmation { .. } => ErrorCode::NeedsConfirmation,
        }
    }

    /// The actionable half of the error. Absent where there is nothing honest
    /// to suggest — a wrong hint is worse than none.
    pub fn hint(&self) -> Option<String> {
        match self {
            Error::Ambiguous { .. } => {
                Some("give more of the id, or pass the full id from `--json`".into())
            }
            Error::Auth(_) => Some(
                "re-run `mailbox accounts test <account>`; the credential may have expired".into(),
            ),
            Error::Conflict(_) => Some("run `mailbox sync --once` and retry".into()),
            Error::NeedsConfirmation { hint, .. } => Some(hint.clone()),
            _ => None,
        }
    }

    pub fn to_response(&self) -> ErrorResponse {
        let r = ErrorResponse::new(self.code(), self.to_string());
        match self.hint() {
            Some(h) => r.with_hint(h),
            None => r,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.code().exit_code()
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Error {
        Error::Store(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Error {
        Error::Store(format!("json: {e}"))
    }
}

impl Error {
    /// Re-labels a usage error as `needs_confirmation` (§6.6 exit code 8), with
    /// the flag that supplies the missing answer.
    ///
    /// A separate constructor rather than a variant field, because the message
    /// is already written by the time a caller knows a confirmation was what
    /// was missing.
    pub fn needs_confirmation(self, hint: impl Into<String>) -> Error {
        Error::NeedsConfirmation {
            message: self.to_string(),
            hint: hint.into(),
        }
    }
}

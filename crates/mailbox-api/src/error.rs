//! The structured error contract (§6.4) and the exit codes it maps to (§6.6).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Every failure the CLI can report. The wire form is the snake_case variant
/// name; adding a variant is additive, renaming one is a `mailbox/v2` change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Catch-all for a failure with no more specific code.
    Failed,
    /// The arguments did not parse, or they contradict each other.
    Usage,
    /// A named message, folder, account, or attachment does not exist.
    NotFound,
    /// An id prefix matched more than one message.
    Ambiguous,
    /// The server rejected our credentials, or we have none.
    AuthFailed,
    /// The network is unreachable, or the server is.
    Network,
    /// Local state disagrees with the server; a resync is needed.
    Conflict,
    /// A `policy.toml` rule refused the operation.
    PolicyDenied,
    /// The command would have prompted, and stdin is not a TTY.
    NeedsConfirmation,
    /// The operation exceeded `--timeout`.
    Timeout,
    /// The store is corrupt, locked by an incompatible version, or unreadable.
    Store,
    /// The server broke the protocol badly enough that we gave up.
    Protocol,
}

impl ErrorCode {
    /// §6.6. `0` is success and therefore never produced here.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Failed | ErrorCode::Store | ErrorCode::Protocol => 1,
            ErrorCode::Usage => 2,
            ErrorCode::NotFound | ErrorCode::Ambiguous => 3,
            ErrorCode::AuthFailed => 4,
            ErrorCode::Network => 5,
            ErrorCode::Conflict => 6,
            ErrorCode::PolicyDenied => 7,
            ErrorCode::NeedsConfirmation => 8,
            ErrorCode::Timeout => 9,
        }
    }

    /// Whether an identical retry could plausibly succeed.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::Network | ErrorCode::Timeout | ErrorCode::Conflict
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Failed => "failed",
            ErrorCode::Usage => "usage",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Ambiguous => "ambiguous",
            ErrorCode::AuthFailed => "auth_failed",
            ErrorCode::Network => "network",
            ErrorCode::Conflict => "conflict",
            ErrorCode::PolicyDenied => "policy_denied",
            ErrorCode::NeedsConfirmation => "needs_confirmation",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Store => "store",
            ErrorCode::Protocol => "protocol",
        }
    }
}

/// The body of a `--json` failure, printed to stderr.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    /// What the caller can do about it — the flag that supplies a missing
    /// answer, the command that repairs the state. Absent when there is
    /// nothing actionable to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The whole stderr document: `{"error": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

impl ErrorResponse {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorResponse {
            error: ErrorBody {
                code,
                message: message.into(),
                retryable: code.retryable(),
                hint: None,
            },
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.error.hint = Some(hint.into());
        self
    }
}

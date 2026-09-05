//! `policy.toml` (§6.5) — the guardrails on sending.
//!
//! > Agents that can send mail unsupervised are a liability. The spec bakes in
//! > the guardrails rather than leaving them to the caller.
//!
//! Everything here is a pure function of a policy and a draft. No clock, no
//! network, no store — except the two counters the caller supplies — so the
//! whole file is testable as a table, which is what a security control needs.
//!
//! A violation names the rule that produced it. "policy_denied" without a rule
//! is a dead end for whoever has to decide whether the policy or the message is
//! wrong.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::compose::Draft;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)]
    pub send: SendPolicy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendPolicy {
    /// Recipient patterns that are allowed. When non-empty, a recipient
    /// matching none of them is denied — an allowlist is a default-deny.
    #[serde(default)]
    pub allow_recipients: Vec<String>,
    /// Recipient patterns that are denied, checked after the allowlist so a
    /// narrow denial can carve a hole in a broad permission.
    #[serde(default)]
    pub deny_recipients: Vec<String>,
    /// Most messages in any rolling hour.
    #[serde(default)]
    pub max_per_hour: Option<u32>,
    /// Most envelope recipients on one message. The guard against a loop that
    /// builds a recipient list from a query.
    #[serde(default)]
    pub max_recipients: Option<usize>,
    /// Domains considered internal. With `confirm_external`, anything outside
    /// them needs `--yes`.
    #[serde(default)]
    pub internal_domains: Vec<String>,
    /// Require explicit confirmation for a recipient outside
    /// `internal_domains`.
    #[serde(default)]
    pub confirm_external: bool,
    /// Largest total attachment payload, in bytes.
    #[serde(default)]
    pub max_attachment_bytes: Option<u64>,
}

/// What the caller knows that the policy cannot work out for itself.
#[derive(Debug, Clone, Default)]
pub struct SendContext {
    /// Messages accepted in the last hour.
    pub sends_last_hour: u32,
    /// Whether the caller passed `--yes`.
    pub confirmed: bool,
}

/// A refused send, naming the rule (§6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The policy key that refused, e.g. `send.deny_recipients`.
    pub rule: String,
    pub detail: String,
}

impl From<Violation> for Error {
    fn from(v: Violation) -> Error {
        Error::PolicyDenied {
            rule: v.rule,
            detail: v.detail,
        }
    }
}

impl Policy {
    pub fn load(path: &Path) -> Result<Policy> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::usage(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load_default() -> Result<Policy> {
        Policy::load(&crate::paths::config_dir().join("policy.toml"))
    }

    /// Checks a draft. `Ok(())` means every rule passed.
    ///
    /// Checks run cheapest-first and stop at the first violation: the caller
    /// gets one clear reason rather than a list to triage.
    pub fn check(&self, draft: &Draft, ctx: &SendContext) -> std::result::Result<(), Violation> {
        let p = &self.send;
        let recipients = draft.envelope_recipients();

        if let Some(max) = p.max_recipients {
            if recipients.len() > max {
                return Err(Violation {
                    rule: "send.max_recipients".into(),
                    detail: format!("{} recipients, limit is {max}", recipients.len()),
                });
            }
        }

        if let Some(max) = p.max_attachment_bytes {
            let total = draft.total_attachment_bytes();
            if total > max {
                return Err(Violation {
                    rule: "send.max_attachment_bytes".into(),
                    detail: format!("{total} bytes of attachments, limit is {max}"),
                });
            }
        }

        if let Some(max) = p.max_per_hour {
            if ctx.sends_last_hour >= max {
                return Err(Violation {
                    rule: "send.max_per_hour".into(),
                    // "queued or sent", because the count includes mail that
                    // has not gone out yet — and a message told it was refused
                    // because 5 were "sent" when the outbox has delivered none
                    // reads as a bug in the client.
                    detail: format!(
                        "{} queued or sent in the last hour, limit is {max}",
                        ctx.sends_last_hour
                    ),
                });
            }
        }

        for rcpt in &recipients {
            if !p.allow_recipients.is_empty()
                && !p.allow_recipients.iter().any(|pat| matches(pat, rcpt))
            {
                return Err(Violation {
                    rule: "send.allow_recipients".into(),
                    detail: format!("{rcpt} matches no allowed pattern"),
                });
            }
            if let Some(pat) = p.deny_recipients.iter().find(|pat| matches(pat, rcpt)) {
                return Err(Violation {
                    rule: "send.deny_recipients".into(),
                    detail: format!("{rcpt} matches denied pattern `{pat}`"),
                });
            }
        }

        if p.confirm_external && !ctx.confirmed {
            if let Some(external) = recipients
                .iter()
                .find(|r| !p.internal_domains.iter().any(|d| in_domain(r, d)))
            {
                return Err(Violation {
                    rule: "send.confirm_external".into(),
                    detail: format!("{external} is outside the internal domains; pass --yes"),
                });
            }
        }

        Ok(())
    }
}

/// Glob matching for a recipient pattern: `*` matches any run of characters.
///
/// Deliberately not a regular expression. A policy file is a security control
/// that has to be readable by whoever has to trust it, and `*@example.com` is
/// obvious in a way that `^.*@example\.com$` is not.
pub fn matches(pattern: &str, address: &str) -> bool {
    glob_match(&pattern.to_ascii_lowercase(), &address.to_ascii_lowercase())
}

fn glob_match(pattern: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), s.chars().collect());
    // Iterative, with backtracking only on `*`, so a pathological pattern
    // cannot blow the stack the way a naive recursion would.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether an address is in a domain, matching subdomains too.
fn in_domain(address: &str, domain: &str) -> bool {
    let Some((_, addr_domain)) = address.rsplit_once('@') else {
        return false;
    };
    let (addr_domain, domain) = (
        addr_domain.to_ascii_lowercase(),
        domain.trim_start_matches('.').to_ascii_lowercase(),
    );
    addr_domain == domain || addr_domain.ends_with(&format!(".{domain}"))
}

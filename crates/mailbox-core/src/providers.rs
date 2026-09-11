//! Where a domain's mail servers are (§9).
//!
//! Two layers, cheapest first. [`hints_for`] is a built-in table covering the
//! hosts almost everyone uses, so adding an account on a phone is "address and
//! password" rather than also typing `imap.gmail.com`; it answers offline and
//! instantly. [`discover`] is for everything else: RFC 6186 `SRV` records,
//! then a guess built from the domain's `MX`, then the conventional names.
//!
//! `autoconfig`/`autodiscover` and the Mozilla ISPDB are still to come; they
//! slot in as further steps inside [`discover`], between the SRV lookup and
//! the guesses.

use crate::dns;
use crate::error::{Error, Result};

/// IMAP and SMTP settings the table knows for one domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHints {
    pub imap_host: String,
    pub imap_port: u16,
    /// `implicit` | `starttls` | `none`.
    pub imap_security: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// `implicit` | `starttls` | `none`.
    pub smtp_security: String,
    /// The login name, when the source said it is not the address.
    pub imap_user: Option<String>,
    pub smtp_user: Option<String>,
}

/// Looks up the domain of `address` in the built-in table.
pub fn hints_for(address: &str) -> Option<ServerHints> {
    let domain = address.rsplit('@').next()?.trim().to_ascii_lowercase();
    if domain.is_empty() || !domain.contains('.') {
        return None;
    }
    hints_for_domain(&domain)
}

fn hints_for_domain(domain: &str) -> Option<ServerHints> {
    match domain {
        "gmail.com" | "googlemail.com" => Some(ServerHints {
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: "implicit".into(),
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 587,
            smtp_security: "starttls".into(),
            imap_user: None,
            smtp_user: None,
        }),
        "fastmail.com" | "fastmail.fm" => Some(ServerHints {
            imap_host: "imap.fastmail.com".into(),
            imap_port: 993,
            imap_security: "implicit".into(),
            smtp_host: "smtp.fastmail.com".into(),
            smtp_port: 587,
            smtp_security: "starttls".into(),
            imap_user: None,
            smtp_user: None,
        }),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "office365.com" => {
            Some(ServerHints {
                imap_host: "outlook.office365.com".into(),
                imap_port: 993,
                imap_security: "implicit".into(),
                smtp_host: "smtp.office365.com".into(),
                smtp_port: 587,
                smtp_security: "starttls".into(),
                imap_user: None,
                smtp_user: None,
            })
        }
        "yahoo.com" | "yahoo.co.uk" | "ymail.com" => Some(ServerHints {
            imap_host: "imap.mail.yahoo.com".into(),
            imap_port: 993,
            imap_security: "implicit".into(),
            smtp_host: "smtp.mail.yahoo.com".into(),
            smtp_port: 587,
            smtp_security: "starttls".into(),
            imap_user: None,
            smtp_user: None,
        }),
        "icloud.com" | "me.com" | "mac.com" => Some(ServerHints {
            imap_host: "imap.mail.icloud.com".into(),
            imap_port: 993,
            imap_security: "implicit".into(),
            smtp_host: "smtp.mail.icloud.com".into(),
            smtp_port: 587,
            smtp_security: "starttls".into(),
            imap_user: None,
            smtp_user: None,
        }),
        "proton.me" | "protonmail.com" | "pm.me" => Some(ServerHints {
            imap_host: "imap.protonmail.ch".into(),
            imap_port: 993,
            imap_security: "implicit".into(),
            smtp_host: "smtp.protonmail.ch".into(),
            smtp_port: 587,
            smtp_security: "starttls".into(),
            imap_user: None,
            smtp_user: None,
        }),
        _ => None,
    }
}

/// How settings were arrived at, so the UI can say so. Someone typing a
/// password into a form that filled itself in deserves to know whether the
/// host came from the domain's own records or from a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The built-in table.
    Table,
    /// The domain's own `SRV` records (RFC 6186).
    Srv,
    /// Built from the domain's `MX`, which is a hint about the operator and
    /// not a statement about where IMAP lives.
    Mx,
    /// The conventional `imap.`/`smtp.` names, tried because nothing better
    /// was published.
    Guess,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Table => "table",
            Source::Srv => "srv",
            Source::Mx => "mx",
            Source::Guess => "guess",
        }
    }

    /// Whether the domain actually published these settings. A guess is worth
    /// showing, but not worth trusting silently.
    pub fn is_authoritative(self) -> bool {
        matches!(self, Source::Table | Source::Srv)
    }
}

/// Settings for a domain, and where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub hints: ServerHints,
    pub imap_source: Source,
    pub smtp_source: Source,
}

/// Finds server settings for `address`, consulting the network.
///
/// The order is table, then `SRV`, then a guess; each step only fills what the
/// one before it left empty, so a domain that publishes `_imaps._tcp` but no
/// `_submission._tcp` still gets a usable SMTP row. Nothing here proves the
/// host is right — the TLS certificate does that, on connect — so the result
/// is a filled-in form, not a configured account.
///
/// Blocking, and it talks to a nameserver. Callers on a UI thread want it on a
/// worker; the phone's setup screen does exactly that.
pub fn discover(address: &str) -> Result<Discovered> {
    let domain = address
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if domain.is_empty() || !domain.contains('.') {
        return Err(Error::Usage(format!(
            "`{address}` has no domain to look up"
        )));
    }

    if let Some(hints) = hints_for_domain(&domain) {
        return Ok(Discovered {
            hints,
            imap_source: Source::Table,
            smtp_source: Source::Table,
        });
    }

    // RFC 6186: the implicit-TLS name first, since that is what we would
    // rather end up on, then the STARTTLS one.
    let imap = first_srv(&[
        (&format!("_imaps._tcp.{domain}"), "implicit"),
        (&format!("_imap._tcp.{domain}"), "starttls"),
    ]);
    let smtp = first_srv(&[
        (&format!("_submissions._tcp.{domain}"), "implicit"),
        (&format!("_submission._tcp.{domain}"), "starttls"),
    ]);

    // A domain that publishes neither may still be one host's worth of mail
    // behind an MX that names the operator — `mx.migadu.com` for a domain
    // Migadu hosts. That is a better guess than the domain itself, but it is
    // still a guess.
    let fallback_base = if imap.is_none() || smtp.is_none() {
        mx_base(&domain).unwrap_or_else(|| domain.clone())
    } else {
        domain.clone()
    };
    let from_mx = fallback_base != domain;
    let guess_source = if from_mx { Source::Mx } else { Source::Guess };

    let (imap_host, imap_port, imap_security, imap_source) = match imap {
        Some((host, port, security)) => (host, port, security, Source::Srv),
        None => (
            format!("imap.{fallback_base}"),
            993,
            "implicit",
            guess_source,
        ),
    };
    let (smtp_host, smtp_port, smtp_security, smtp_source) = match smtp {
        Some((host, port, security)) => (host, port, security, Source::Srv),
        None => (
            format!("smtp.{fallback_base}"),
            587,
            "starttls",
            guess_source,
        ),
    };

    Ok(Discovered {
        hints: ServerHints {
            imap_host,
            imap_port,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_security,
        },
        imap_source,
        smtp_source,
    })
}

/// Tries each name in turn, returning the first that answers with a record.
///
/// A lookup that fails is treated like one that found nothing: discovery is
/// best-effort, and one refused query should not deny the user the other
/// three answers.
fn first_srv(names: &[(&str, &'static str)]) -> Option<(String, u16, &'static str)> {
    for (name, security) in names {
        let Ok(records) = dns::srv(name) else {
            continue;
        };
        if let Some(best) = records.into_iter().next() {
            return Some((best.target, best.port, security));
        }
    }
    None
}

/// The registrable-looking part of the domain's best `MX`, or `None` when
/// there is no MX or it is the domain itself.
fn mx_base(domain: &str) -> Option<String> {
    let best = dns::mx(domain).ok()?.into_iter().next()?;
    let exchange = best.exchange;
    if exchange == domain || exchange.ends_with(&format!(".{domain}")) {
        return None;
    }
    // `mx1.mail.example.net` hosts mail for the operator `example.net`; the
    // last two labels are the part worth prefixing `imap.` onto.
    let labels: Vec<&str> = exchange.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    Some(labels[labels.len() - 2..].join("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_is_known() {
        let h = hints_for("Peter@Gmail.com").expect("gmail");
        assert_eq!(h.imap_host, "imap.gmail.com");
        assert_eq!(h.imap_port, 993);
        assert_eq!(h.smtp_host, "smtp.gmail.com");
    }

    #[test]
    fn unknown_domain_is_none() {
        assert!(hints_for("peter@example.com").is_none());
        assert!(hints_for("not-an-address").is_none());
        assert!(hints_for("").is_none());
    }

    #[test]
    fn discovery_answers_from_the_table_without_asking_the_network() {
        let d = discover("peter@fastmail.com").expect("fastmail");
        assert_eq!(d.hints.imap_host, "imap.fastmail.com");
        assert_eq!(d.imap_source, Source::Table);
        assert!(d.imap_source.is_authoritative());
    }

    #[test]
    fn discovery_needs_a_domain() {
        assert!(discover("not-an-address").is_err());
        assert!(discover("peter@localhost").is_err());
        assert!(discover("").is_err());
    }

    #[test]
    fn a_guess_is_not_authoritative() {
        assert!(!Source::Mx.is_authoritative());
        assert!(!Source::Guess.is_authoritative());
        assert_eq!(Source::Srv.as_str(), "srv");
    }
}

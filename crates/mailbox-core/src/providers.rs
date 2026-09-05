//! Built-in server settings for the big providers (§9).
//!
//! Full autoconfig / DNS SRV / ISPDB discovery lands with `accounts add`.
//! Until then a small table covers the hosts almost everyone uses, so adding
//! an account on a phone is "address and password" rather than also typing
//! `imap.gmail.com`. Unknown domains return `None` and the UI asks for hosts.

/// IMAP and SMTP settings the table knows for one domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHints {
    pub imap_host: String,
    pub imap_port: u16,
    /// `implicit` | `starttls` | `none`.
    pub imap_security: &'static str,
    pub smtp_host: String,
    pub smtp_port: u16,
    /// `implicit` | `starttls` | `none`.
    pub smtp_security: &'static str,
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
            imap_security: "implicit",
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 587,
            smtp_security: "starttls",
        }),
        "fastmail.com" | "fastmail.fm" => Some(ServerHints {
            imap_host: "imap.fastmail.com".into(),
            imap_port: 993,
            imap_security: "implicit",
            smtp_host: "smtp.fastmail.com".into(),
            smtp_port: 587,
            smtp_security: "starttls",
        }),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com" | "office365.com" => {
            Some(ServerHints {
                imap_host: "outlook.office365.com".into(),
                imap_port: 993,
                imap_security: "implicit",
                smtp_host: "smtp.office365.com".into(),
                smtp_port: 587,
                smtp_security: "starttls",
            })
        }
        "yahoo.com" | "yahoo.co.uk" | "ymail.com" => Some(ServerHints {
            imap_host: "imap.mail.yahoo.com".into(),
            imap_port: 993,
            imap_security: "implicit",
            smtp_host: "smtp.mail.yahoo.com".into(),
            smtp_port: 587,
            smtp_security: "starttls",
        }),
        "icloud.com" | "me.com" | "mac.com" => Some(ServerHints {
            imap_host: "imap.mail.icloud.com".into(),
            imap_port: 993,
            imap_security: "implicit",
            smtp_host: "smtp.mail.icloud.com".into(),
            smtp_port: 587,
            smtp_security: "starttls",
        }),
        "proton.me" | "protonmail.com" | "pm.me" => Some(ServerHints {
            imap_host: "imap.protonmail.ch".into(),
            imap_port: 993,
            imap_security: "implicit",
            smtp_host: "smtp.protonmail.ch".into(),
            smtp_port: 587,
            smtp_security: "starttls",
        }),
        _ => None,
    }
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
}

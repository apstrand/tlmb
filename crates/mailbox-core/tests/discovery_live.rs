//! Discovery against the real DNS, for the domains whose records make the
//! interesting cases (§9).
//!
//! Ignored by default: it needs a nameserver and the answers belong to someone
//! else, so it is a thing to run deliberately when changing `dns` or
//! `providers`, not something CI should depend on.

use mailbox_core::providers::{self, Source};

#[test]
#[ignore = "queries real DNS; run deliberately"]
fn srv_records_are_found_and_preferred() {
    // Fastmail publishes RFC 6186 records for the domains it hosts.
    let d = providers::discover("someone@fastmail.com.au").expect("discover");
    println!("{d:#?}");
    assert_eq!(d.imap_source, Source::Srv);
    assert_eq!(d.hints.imap_host, "imap.fastmail.com");
    assert_eq!(d.hints.imap_port, 993);
    assert_eq!(d.smtp_source, Source::Srv);
}

#[test]
#[ignore = "queries real DNS; run deliberately"]
fn a_domain_without_srv_falls_back_to_a_guess() {
    let d = providers::discover("someone@example.com").expect("discover");
    println!("{d:#?}");
    assert!(!d.imap_source.is_authoritative());
}

//! **L3 — the adversarial server** (§14.2).
//!
//! > L3 is our own server, in-tree, because real servers are too well-behaved
//! > to reach the recovery paths.
//!
//! Dovecot will not truncate a literal for you, and no real MTA will accept a
//! message and then hang up before saying so. Those are the paths where mail
//! gets lost or duplicated, and the only way to test them is to build a server
//! that does it on request.
//!
//! Nothing here is a dependency of the shipped binary: this crate exists to be
//! a `dev-dependency` of the test suites.

pub mod smtp;

pub use smtp::{Fault, ReceivedMessage, SmtpServer, Stage};

//! The query language of §6.2 — one grammar for the CLI, the TUI filter bar,
//! the GUI search box, and the API.
//!
//! ```text
//! from:alice to:me subject:"quarterly report"
//! is:unread is:flagged has:attachment
//! in:inbox in:archive tag:invoices thread:<id>
//! before:2026-08-01 after:-7d larger:5M filename:*.pdf
//! alice AND (invoice OR receipt) NOT is:spam
//! ```
//!
//! The rule that shapes the whole module: **an unknown key is an error, not a
//! silent full-text fallback.** `form:alice` must not quietly search for the
//! word "form:alice" and return plausible-looking wrong results — an agent has
//! no way to notice that happened.

pub mod parse;
pub mod sql;

pub use parse::{parse, ParseError};
pub use sql::{compile, CompileOptions, Compiled};

use crate::clock::Timestamp;

/// A parsed query. Bare words become [`Node::Text`]; everything else is a typed
/// term, so the compiler never has to re-inspect a string.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// Matches everything. What an empty query parses to.
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),

    /// A bare word or quoted phrase: full-text over headers and body.
    Text(String),

    /// `from:`, `to:`, `cc:`, `bcc:`. Matches display name or address,
    /// substring, case-insensitively.
    Addr {
        field: AddrField,
        value: String,
    },
    /// `subject:` — substring, case-insensitive.
    Subject(String),

    /// `is:unread`, `is:flagged`, … . `want` is false for `is:read`, which is
    /// sugar for `NOT is:unread`.
    Flag {
        name: String,
        want: bool,
    },
    /// `has:attachment`.
    HasAttachment,

    /// `in:` — folder path, its last segment, or a special-use name.
    Folder(String),
    /// `tag:` — a local tag (§5.4).
    Tag(String),
    /// `thread:` — every message in the named thread.
    Thread(String),
    /// `account:` — restricts the unified inbox to one account (§7).
    Account(String),
    /// `id:` — an exact id or an unambiguous prefix.
    Id(String),

    /// `before:` / `after:`, resolved to an absolute instant at parse time
    /// against the injected clock, so `after:-7d` means the same thing to every
    /// consumer of one parse.
    Date {
        op: DateOp,
        ts: Timestamp,
    },
    /// `larger:` / `smaller:`, in bytes.
    Size {
        op: SizeOp,
        bytes: u64,
    },
    /// `filename:` — a glob against attachment filenames.
    Filename(String),

    /// `raw:` — passed through to the server's own `SEARCH` where it supports
    /// one (§6.2). It matches nothing locally, so a query containing it is only
    /// meaningful against a connected account.
    Raw(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrField {
    From,
    To,
    Cc,
    Bcc,
    /// `to:` in its useful sense — To, Cc, or Bcc, which is what a human means
    /// by "sent to me".
    Recipient,
}

impl AddrField {
    pub fn columns(self) -> &'static [&'static str] {
        match self {
            AddrField::From => &["from"],
            AddrField::To => &["to"],
            AddrField::Cc => &["cc"],
            AddrField::Bcc => &["bcc"],
            AddrField::Recipient => &["to", "cc", "bcc"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateOp {
    /// Strictly before the instant.
    Before,
    /// At or after the instant.
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeOp {
    Larger,
    Smaller,
}

impl Node {
    /// Folds the trivial cases so the compiler and the golden files see one
    /// canonical shape: `AND` of one is that one, `AND` of nothing is `All`.
    pub fn simplify(self) -> Node {
        match self {
            Node::And(v) => {
                let mut out: Vec<Node> = Vec::new();
                for n in v {
                    match n.simplify() {
                        Node::All => {}
                        Node::And(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => Node::All,
                    1 => out.pop().unwrap(),
                    _ => Node::And(out),
                }
            }
            Node::Or(v) => {
                let mut out: Vec<Node> = Vec::new();
                for n in v {
                    match n.simplify() {
                        Node::Or(inner) => out.extend(inner),
                        other => out.push(other),
                    }
                }
                match out.len() {
                    0 => Node::All,
                    1 => out.pop().unwrap(),
                    _ => Node::Or(out),
                }
            }
            Node::Not(n) => match n.simplify() {
                // `NOT NOT x` is `x`, and users write it by combining `-` with
                // an explicit `NOT`.
                Node::Not(inner) => *inner,
                other => Node::Not(Box::new(other)),
            },
            other => other,
        }
    }

    /// Collects every `raw:` term, for the sync engine to hand to the server.
    pub fn raw_terms(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.visit(&mut |n| {
            if let Node::Raw(s) = n {
                out.push(s.as_str());
            }
        });
        out
    }

    pub fn visit<'a>(&'a self, f: &mut impl FnMut(&'a Node)) {
        f(self);
        match self {
            Node::And(v) | Node::Or(v) => v.iter().for_each(|n| n.visit(f)),
            Node::Not(n) => n.visit(f),
            _ => {}
        }
    }
}

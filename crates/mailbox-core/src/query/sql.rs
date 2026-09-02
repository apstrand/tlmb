//! Compiles a [`Node`] into a SQL predicate over the store's `messages` table.
//!
//! Everything is a bound parameter — there is no path by which a query string
//! reaches SQLite as text. That matters more here than in most places, because
//! a query is the one piece of untrusted-ish input that both a human and an
//! agent hand us verbatim.

use rusqlite::types::Value;

use crate::query::{AddrField, DateOp, Node, SizeOp};

/// What the store needs to resolve terms whose meaning depends on context.
#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    /// This store's account id, for `account:`.
    pub account: String,
    /// Every address this account answers to, for `to:me` (§6.2).
    pub identities: Vec<String>,
}

/// A SQL fragment and its parameters, ready to splice into a `WHERE` clause.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub sql: String,
    pub params: Vec<Value>,
}

pub fn compile(node: &Node, opts: &CompileOptions) -> Compiled {
    let mut c = Compiler {
        params: Vec::new(),
        opts,
    };
    let sql = c.node(node);
    Compiled {
        sql,
        params: c.params,
    }
}

struct Compiler<'a> {
    params: Vec<Value>,
    opts: &'a CompileOptions,
}

impl Compiler<'_> {
    fn push(&mut self, v: impl Into<Value>) -> String {
        self.params.push(v.into());
        "?".to_string()
    }

    fn node(&mut self, n: &Node) -> String {
        match n {
            Node::All => "1".to_string(),

            Node::And(v) => {
                let parts: Vec<String> = v.iter().map(|x| self.node(x)).collect();
                format!("({})", parts.join(" AND "))
            }
            Node::Or(v) => {
                let parts: Vec<String> = v.iter().map(|x| self.node(x)).collect();
                format!("({})", parts.join(" OR "))
            }
            // `NOT x` must also exclude rows where `x` is unknown, which for a
            // NULL-bearing column would leave them out of both sides. COALESCE
            // keeps the negation total.
            Node::Not(x) => {
                let inner = self.node(x);
                format!("(NOT COALESCE({inner}, 0))")
            }

            Node::Text(s) => {
                let p = self.push(fts_phrase(s));
                format!(
                    "(m.rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH {p}))"
                )
            }

            Node::Addr { field, value } => self.addr(*field, value),

            Node::Subject(s) => {
                let p = self.push(like_contains(s));
                format!("(m.subject LIKE {p} ESCAPE '\\')")
            }

            // Flags are stored space-delimited and space-padded, so a
            // containment test on ` seen ` cannot match `unseen`.
            Node::Flag { name, want } => {
                let p = self.push(format!(" {} ", name.to_ascii_lowercase()));
                if *want {
                    format!("(instr(m.flags, {p}) > 0)")
                } else {
                    format!("(instr(m.flags, {p}) = 0)")
                }
            }

            Node::HasAttachment => "(m.has_attachment = 1)".to_string(),

            // A folder matches by full path, by leaf name, or by special-use —
            // `in:inbox` should work whether the server calls it `INBOX` or
            // `INBOX.stuff`'s parent, and `in:archive` should find whatever the
            // server flagged `\Archive`.
            Node::Folder(f) => {
                let p1 = self.push(f.clone());
                let p2 = self.push(f.clone());
                let p3 = self.push(f.to_ascii_lowercase());
                format!(
                    "(EXISTS (SELECT 1 FROM placements p JOIN folders fo ON fo.id = p.folder_id \
                      WHERE p.message_rowid = m.rowid AND (fo.path = {p1} COLLATE NOCASE \
                      OR fo.name = {p2} COLLATE NOCASE OR fo.special_use = {p3})))"
                )
            }

            Node::Tag(t) => {
                let p = self.push(t.to_ascii_lowercase());
                format!("(EXISTS (SELECT 1 FROM tags t WHERE t.message_rowid = m.rowid AND t.tag = {p}))")
            }

            // Ids and thread ids accept any unambiguous prefix (§5.2). The
            // caller is responsible for turning "matched more than one" into an
            // `ambiguous` error; the predicate itself just matches.
            Node::Thread(t) => {
                let p = self.push(format!("{}%", escape_like(t)));
                format!("(m.thread_id LIKE {p} ESCAPE '\\')")
            }
            Node::Id(i) => {
                let p = self.push(format!("{}%", escape_like(i)));
                format!("(m.id LIKE {p} ESCAPE '\\')")
            }

            // One store holds one account (§5.1), so this is decided at compile
            // time rather than costing a column comparison per row.
            Node::Account(a) => if a.eq_ignore_ascii_case(&self.opts.account) {
                "1"
            } else {
                "0"
            }
            .to_string(),

            Node::Date { op, ts } => {
                let p = self.push(*ts);
                match op {
                    DateOp::Before => format!("(m.date < {p})"),
                    DateOp::After => format!("(m.date >= {p})"),
                }
            }

            Node::Size { op, bytes } => {
                let p = self.push(*bytes as i64);
                match op {
                    SizeOp::Larger => format!("(m.size > {p})"),
                    SizeOp::Smaller => format!("(m.size < {p})"),
                }
            }

            Node::Filename(glob) => {
                let p = self.push(glob.to_ascii_lowercase());
                format!(
                    "(EXISTS (SELECT 1 FROM attachments a WHERE a.message_rowid = m.rowid \
                      AND a.filename_lc GLOB {p}))"
                )
            }

            // `raw:` is a server-side term (§6.2). It cannot be evaluated
            // locally, and pretending otherwise would silently drop the
            // restriction, so it matches everything here and the sync engine
            // intersects the server's answer.
            Node::Raw(_) => "1".to_string(),
        }
    }

    fn addr(&mut self, field: AddrField, value: &str) -> String {
        let fields: Vec<String> = field
            .columns()
            .iter()
            .map(|c| self.push((*c).to_string()))
            .collect();

        // `to:me` and `from:me` mean "any address this account answers to".
        let value_pred = if value.eq_ignore_ascii_case("me") && !self.opts.identities.is_empty() {
            let ids: Vec<String> = self
                .opts
                .identities
                .iter()
                .map(|i| self.push(i.to_ascii_lowercase()))
                .collect();
            format!("a.addr IN ({})", ids.join(", "))
        } else {
            let p1 = self.push(like_contains(value));
            let p2 = self.push(like_contains(value));
            format!("(a.addr LIKE {p1} ESCAPE '\\' OR a.name LIKE {p2} ESCAPE '\\')")
        };

        format!(
            "(EXISTS (SELECT 1 FROM addresses a WHERE a.message_rowid = m.rowid \
              AND a.field IN ({}) AND {value_pred}))",
            fields.join(", ")
        )
    }
}

/// Wraps a term as an FTS5 phrase.
///
/// FTS5's own query syntax has operators (`AND`, `NEAR`, `*`, `^`, `-`) that a
/// user's bare word would otherwise trip. Ours is the query language the user
/// is writing in (§6.2); FTS5's is an implementation detail, and letting it
/// leak would make `NEAR` unsearchable and `foo*` mean something the grammar
/// never promised.
pub fn fts_phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn like_contains(s: &str) -> String {
    format!("%{}%", escape_like(s))
}

/// `%` and `_` are wildcards in `LIKE`; a user searching for `50%_off` means
/// those literally.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

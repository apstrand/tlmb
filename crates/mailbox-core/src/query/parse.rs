//! Lexer and recursive-descent parser for the §6.2 grammar.
//!
//! Precedence, loosest to tightest: `OR`, implicit `AND`, `NOT`/`-`, primary.
//! Adjacency means `AND`, so `from:alice invoice` is a conjunction, which is
//! what every mail search in the world does and what users expect.

use crate::clock::{Clock, Timestamp};
use crate::query::{AddrField, DateOp, Node, SizeOp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Byte offset into the query, for a caret in a human-facing error.
    pub offset: usize,
    /// Present when the failure was an unknown key, so the CLI can suggest.
    pub unknown_key: Option<String>,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at offset {})", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for crate::Error {
    fn from(e: ParseError) -> crate::Error {
        crate::Error::Usage(e.to_string())
    }
}

/// Every key the grammar accepts. Listed once, so the error path can print the
/// full set and the "did you mean" can be computed rather than maintained.
pub const KEYS: &[&str] = &[
    "from", "to", "cc", "bcc", "subject", "is", "has", "in", "folder", "tag", "thread", "account",
    "id", "before", "after", "larger", "smaller", "filename", "raw",
];

/// Values `is:` accepts. `is:read` and `is:seen` are the same thing said two
/// ways, and both appear in muscle memory from other clients.
pub const IS_VALUES: &[&str] = &[
    "unread",
    "read",
    "seen",
    "unseen",
    "flagged",
    "unflagged",
    "snoozed",
    "answered",
    "draft",
    "deleted",
    "spam",
    "junk",
];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare or quoted word. `quoted` suppresses operator and key
    /// interpretation, so `"AND"` searches for the word.
    Word {
        text: String,
        quoted: bool,
        offset: usize,
    },
    /// `key:value`, already split. The value keeps its own quoting flag.
    Term {
        key: String,
        value: String,
        offset: usize,
    },
    And,
    Or,
    Not,
    LParen(usize),
    RParen(usize),
}

/// Parses a query string against `clock`, which resolves relative dates.
///
/// An empty or all-whitespace query is [`Node::All`], not an error — `mailbox
/// ls` with no arguments goes through here.
pub fn parse(input: &str, clock: &Clock) -> Result<Node, ParseError> {
    let toks = lex(input)?;
    let mut p = Parser {
        toks,
        pos: 0,
        clock,
        input,
    };
    let node = p.parse_or()?;
    if p.pos < p.toks.len() {
        let message = match p.toks.get(p.pos) {
            Some(Tok::RParen(_)) => "unmatched ')'",
            _ => "unexpected trailing input",
        };
        return Err(p.err_at(p.pos, message));
    }
    Ok(node.simplify())
}

fn lex(input: &str) -> Result<Vec<Tok>, ParseError> {
    let b = input.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();

    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'(' {
            out.push(Tok::LParen(i));
            i += 1;
            continue;
        }
        if c == b')' {
            out.push(Tok::RParen(i));
            i += 1;
            continue;
        }
        // `-term` is negation; a bare `-` or a `-` inside a word is not.
        if c == b'-' && i + 1 < b.len() && !b[i + 1].is_ascii_whitespace() && b[i + 1] != b')' {
            out.push(Tok::Not);
            i += 1;
            continue;
        }

        let start = i;

        // A key is recognised *before* the value is read, so
        // `subject:"quarterly report"` keeps its key while `"subject:x"` — where
        // the quote comes first — is a phrase. Deciding this after reading the
        // whole word cannot distinguish the two.
        if let Some(colon) = key_prefix(input, i) {
            let key = input[i..colon].to_ascii_lowercase();
            let (value, _, next) = read_word(input, colon + 1)?;
            i = next;
            out.push(Tok::Term {
                key,
                value,
                offset: start,
            });
            continue;
        }

        let (word, quoted, next) = read_word(input, i)?;
        i = next;

        if !quoted {
            match word.to_ascii_uppercase().as_str() {
                "AND" => {
                    out.push(Tok::And);
                    continue;
                }
                "OR" => {
                    out.push(Tok::Or);
                    continue;
                }
                "NOT" => {
                    out.push(Tok::Not);
                    continue;
                }
                _ => {}
            }
        }

        out.push(Tok::Word {
            text: word,
            quoted,
            offset: start,
        });
    }
    Ok(out)
}

/// Returns the index of the `:` when the input at `from` begins with a bare
/// alphabetic key followed by a colon.
///
/// Only ASCII letters, and only unquoted: `"re: lunch"` must stay a phrase, and
/// a leading quote means the whole token was quoted deliberately.
fn key_prefix(input: &str, from: usize) -> Option<usize> {
    let b = input.as_bytes();
    let mut i = from;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        i += 1;
    }
    (i > from && i < b.len() && b[i] == b':').then_some(i)
}

/// Reads one whitespace-delimited word, honouring double quotes anywhere in it,
/// so `subject:"a b"` comes back as one token.
fn read_word(input: &str, from: usize) -> Result<(String, bool, usize), ParseError> {
    let b = input.as_bytes();
    let mut i = from;
    let mut out = String::new();
    let mut in_quotes = false;
    let mut saw_quotes = false;

    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            in_quotes = !in_quotes;
            saw_quotes = true;
            i += 1;
            continue;
        }
        if c == b'\\' && i + 1 < b.len() {
            // Escapes exist so a literal quote or paren can be searched for.
            out.push(b[i + 1] as char);
            i += 2;
            continue;
        }
        if !in_quotes && (c.is_ascii_whitespace() || c == b'(' || c == b')') {
            break;
        }
        // Multi-byte UTF-8 passes through whole.
        let ch_len = utf8_len(c);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }

    if in_quotes {
        return Err(ParseError {
            message: "unterminated quoted string".into(),
            offset: from,
            unknown_key: None,
        });
    }
    Ok((out, saw_quotes, i))
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    clock: &'a Clock,
    input: &'a str,
}

impl Parser<'_> {
    fn parse_or(&mut self) -> Result<Node, ParseError> {
        let mut terms = vec![self.parse_and()?];
        while matches!(self.toks.get(self.pos), Some(Tok::Or)) {
            self.pos += 1;
            terms.push(self.parse_and()?);
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            Node::Or(terms)
        })
    }

    fn parse_and(&mut self) -> Result<Node, ParseError> {
        let mut terms = Vec::new();
        loop {
            match self.toks.get(self.pos) {
                None | Some(Tok::RParen(_)) | Some(Tok::Or) => break,
                // An explicit AND between two terms that were already adjacent.
                Some(Tok::And) => {
                    if terms.is_empty() {
                        return Err(self.err_at(self.pos, "AND with nothing on its left"));
                    }
                    self.pos += 1;
                    continue;
                }
                _ => terms.push(self.parse_unary()?),
            }
        }
        if terms.is_empty() {
            return Ok(Node::All);
        }
        Ok(if terms.len() == 1 {
            terms.pop().unwrap()
        } else {
            Node::And(terms)
        })
    }

    fn parse_unary(&mut self) -> Result<Node, ParseError> {
        if matches!(self.toks.get(self.pos), Some(Tok::Not)) {
            self.pos += 1;
            return Ok(Node::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Node, ParseError> {
        match self.toks.get(self.pos).cloned() {
            Some(Tok::LParen(_)) => {
                self.pos += 1;
                let inner = self.parse_or()?;
                match self.toks.get(self.pos) {
                    Some(Tok::RParen(_)) => {
                        self.pos += 1;
                        Ok(inner)
                    }
                    _ => Err(self.err_at(self.pos, "missing ')'")),
                }
            }
            Some(Tok::Word { text, quoted, .. }) => {
                self.pos += 1;
                // Quoting defeats it, as it defeats the operators: `"foo*"`
                // searches for a word with an asterisk in it.
                match text.strip_suffix('*') {
                    Some(stem) if !quoted && !stem.is_empty() => {
                        Ok(Node::TextPrefix(stem.to_string()))
                    }
                    _ => Ok(Node::Text(text)),
                }
            }
            Some(Tok::Term { key, value, offset }) => {
                self.pos += 1;
                self.term(&key, &value, offset)
            }
            Some(Tok::RParen(_)) => Err(self.err_at(self.pos, "unmatched ')'")),
            Some(t) => Err(self.err_at(self.pos, format!("unexpected {t:?}"))),
            None => Err(self.err_at(self.pos, "unexpected end of query")),
        }
    }

    fn term(&self, key: &str, value: &str, offset: usize) -> Result<Node, ParseError> {
        let err = |msg: String| ParseError {
            message: msg,
            offset,
            unknown_key: None,
        };

        if value.is_empty() && key != "raw" {
            return Err(err(format!("`{key}:` needs a value")));
        }

        Ok(match key {
            "from" => Node::Addr {
                field: AddrField::From,
                value: value.to_string(),
            },
            "to" => Node::Addr {
                field: AddrField::Recipient,
                value: value.to_string(),
            },
            "cc" => Node::Addr {
                field: AddrField::Cc,
                value: value.to_string(),
            },
            "bcc" => Node::Addr {
                field: AddrField::Bcc,
                value: value.to_string(),
            },
            "subject" => Node::Subject(value.to_string()),
            "in" | "folder" => Node::Folder(value.to_string()),
            "tag" => Node::Tag(value.to_string()),
            "thread" => Node::Thread(strip_brackets(value)),
            "account" => Node::Account(value.to_string()),
            "id" => Node::Id(strip_brackets(value)),
            "filename" => Node::Filename(value.to_string()),
            "raw" => Node::Raw(value.to_string()),

            "has" => match value.to_ascii_lowercase().as_str() {
                "attachment" | "attachments" => Node::HasAttachment,
                other => {
                    return Err(err(format!(
                        "`has:{other}` is not a thing; only `has:attachment`"
                    )))
                }
            },

            "is" => {
                let v = value.to_ascii_lowercase();
                match v.as_str() {
                    "unread" | "unseen" => Node::Flag {
                        name: "seen".into(),
                        want: false,
                    },
                    "read" | "seen" => Node::Flag {
                        name: "seen".into(),
                        want: true,
                    },
                    "unflagged" => Node::Flag {
                        name: "flagged".into(),
                        want: false,
                    },
                    "junk" => Node::Flag {
                        name: "spam".into(),
                        want: true,
                    },
                    // Not a flag: a local row and a deadline (§7). Resolved
                    // here against the injected clock so the whole query means
                    // one thing, exactly as `before:` and `after:` are.
                    "snoozed" => Node::Snoozed(self.clock.now()),
                    other if IS_VALUES.contains(&other) => Node::Flag {
                        name: other.to_string(),
                        want: true,
                    },
                    other => {
                        return Err(err(format!(
                            "unknown `is:` value `{other}`; try one of: {}",
                            IS_VALUES.join(", ")
                        )))
                    }
                }
            }

            "before" => Node::Date {
                op: DateOp::Before,
                ts: parse_date(value, self.clock)
                    .ok_or_else(|| err(format!("`before:{value}` is not a date")))?,
            },
            "after" => Node::Date {
                op: DateOp::After,
                ts: parse_date(value, self.clock)
                    .ok_or_else(|| err(format!("`after:{value}` is not a date")))?,
            },

            "larger" => Node::Size {
                op: SizeOp::Larger,
                bytes: parse_size(value)
                    .ok_or_else(|| err(format!("`larger:{value}` is not a size")))?,
            },
            "smaller" => Node::Size {
                op: SizeOp::Smaller,
                bytes: parse_size(value)
                    .ok_or_else(|| err(format!("`smaller:{value}` is not a size")))?,
            },

            unknown => {
                // §6.2: this is where the spec's hard line lives. Falling back
                // to full-text here would return results that look right.
                return Err(ParseError {
                    message: format!(
                        "unknown search key `{unknown}:`{}. Known keys: {}",
                        match nearest_key(unknown) {
                            Some(k) => format!(" — did you mean `{k}:`?"),
                            None => String::new(),
                        },
                        KEYS.join(", ")
                    ),
                    offset,
                    unknown_key: Some(unknown.to_string()),
                });
            }
        })
    }

    fn err_at(&self, tokpos: usize, msg: impl Into<String>) -> ParseError {
        let offset = self
            .toks
            .get(tokpos)
            .and_then(tok_offset)
            .unwrap_or(self.input.len());
        ParseError {
            message: msg.into(),
            offset,
            unknown_key: None,
        }
    }
}

fn tok_offset(t: &Tok) -> Option<usize> {
    match t {
        Tok::Word { offset, .. } | Tok::Term { offset, .. } => Some(*offset),
        Tok::LParen(o) | Tok::RParen(o) => Some(*o),
        _ => None,
    }
}

fn strip_brackets(s: &str) -> String {
    s.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_string()
}

/// Single-edit-distance neighbour among the known keys — enough for a typo,
/// not enough to guess wildly.
fn nearest_key(input: &str) -> Option<&'static str> {
    KEYS.iter()
        .copied()
        .find(|k| edit_distance_at_most_1(k, input))
}

/// One insertion, deletion, substitution, or *transposition*.
///
/// The transposition case is not optional: `form:` for `from:` is the single
/// most common typo in this grammar, and it is two Levenshtein edits.
fn edit_distance_at_most_1(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    if a.len() == b.len() {
        let diffs: Vec<usize> = (0..a.len())
            .filter(|&i| !a[i].eq_ignore_ascii_case(&b[i]))
            .collect();
        if diffs.len() == 2
            && diffs[1] == diffs[0] + 1
            && a[diffs[0]].eq_ignore_ascii_case(&b[diffs[1]])
            && a[diffs[1]].eq_ignore_ascii_case(&b[diffs[0]])
        {
            return true;
        }
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut i = 0;
    let mut j = 0;
    let mut diffs = 0;
    while i < short.len() && j < long.len() {
        if short[i].eq_ignore_ascii_case(&long[j]) {
            i += 1;
            j += 1;
        } else {
            diffs += 1;
            if diffs > 1 {
                return false;
            }
            if short.len() == long.len() {
                i += 1;
            }
            j += 1;
        }
    }
    diffs + (long.len() - j) + (short.len() - i) <= 1
}

/// `2026-08-01`, `2026-08-01T10:00:00Z`, `-7d`, `today`, `yesterday`,
/// `last-week`, or bare unix seconds.
///
/// Relative forms resolve against the injected clock, and a bare date resolves
/// to midnight UTC — not local midnight, because a query that means something
/// different depending on where the laptop is cannot be golden-file tested and
/// surprises anyone who travels.
pub fn parse_date(s: &str, clock: &Clock) -> Option<Timestamp> {
    let s = s.trim();
    let now = clock.now();

    match s.to_ascii_lowercase().as_str() {
        "today" => return Some(midnight_utc(now)),
        "yesterday" => return Some(midnight_utc(now) - 86_400),
        "tomorrow" => return Some(midnight_utc(now) + 86_400),
        "last-week" | "lastweek" => return Some(midnight_utc(now) - 7 * 86_400),
        "last-month" | "lastmonth" => return Some(midnight_utc(now) - 30 * 86_400),
        "last-year" | "lastyear" => return Some(midnight_utc(now) - 365 * 86_400),
        _ => {}
    }

    // `-7d`, `-2w`, `+1d`. The sign is required, so `7d` is a parse error
    // rather than an ambiguous "seven days ago or hence".
    if let Some(rest) = s.strip_prefix('-').or_else(|| s.strip_prefix('+')) {
        let sign: i64 = if s.starts_with('-') { -1 } else { 1 };
        let (num, unit) = rest.split_at(rest.len().saturating_sub(1));
        if let Ok(n) = num.parse::<i64>() {
            let secs = match unit {
                "s" => 1,
                "m" => 60,
                "h" => 3_600,
                "d" => 86_400,
                "w" => 7 * 86_400,
                "y" => 365 * 86_400,
                _ => return None,
            };
            return Some(now + sign * n * secs);
        }
        return None;
    }

    if let Ok(dt) = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
        return Some(dt.unix_timestamp());
    }
    // Bare `YYYY-MM-DD`.
    let fmt = time::macros::format_description!("[year]-[month]-[day]");
    if let Ok(d) = time::Date::parse(s, fmt) {
        return Some(d.midnight().assume_utc().unix_timestamp());
    }
    if s.len() >= 9 {
        if let Ok(n) = s.parse::<i64>() {
            return Some(n);
        }
    }
    None
}

fn midnight_utc(ts: Timestamp) -> Timestamp {
    ts - ts.rem_euclid(86_400)
}

/// `5M`, `500k`, `1G`, `1234`. Decimal-ish `KB`/`MB` spellings are accepted and
/// mean the same as the bare letter: binary multiples, because that is what
/// every mail client that offers this has always meant.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().ok()?;
    let mult = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };
    n.checked_mul(mult)
}

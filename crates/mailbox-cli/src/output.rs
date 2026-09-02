//! Output: one JSON document, one JSON object per line, or something a human
//! wants to read (§6.3, §6.4).
//!
//! The two audiences are served by the same code path on purpose. Every
//! end-to-end test drives the CLI and asserts on `--json`, so the test suite is
//! the largest consumer of the agent contract — and any breakage of it fails a
//! build rather than a user (§14.2).

use std::io::{IsTerminal, Write};

use mailbox_api::{ErrorResponse, Reply, SCHEMA_VERSION};
use serde::Serialize;

use crate::cli::{ColorChoice, GlobalFlags};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Human,
    Json,
    Ndjson,
}

pub struct Out {
    pub mode: Mode,
    pub color: bool,
    pub width: usize,
    pub quiet: bool,
    /// `--fields a,b,c`: restrict JSON objects to these keys.
    fields: Option<Vec<String>>,
}

impl Out {
    pub fn new(flags: &GlobalFlags) -> Out {
        let stdout_is_tty = std::io::stdout().is_terminal();
        let mode = if flags.ndjson {
            Mode::Ndjson
        } else if flags.json {
            Mode::Json
        } else {
            Mode::Human
        };
        Out {
            mode,
            // §6.3: NO_COLOR is honoured, and structured output never carries
            // ANSI (§6.4) whatever the terminal is.
            color: match flags.color {
                ColorChoice::Always => mode == Mode::Human,
                ColorChoice::Never => false,
                ColorChoice::Auto => {
                    mode == Mode::Human && stdout_is_tty && std::env::var_os("NO_COLOR").is_none()
                }
            },
            width: terminal_width(stdout_is_tty),
            quiet: flags.quiet,
            fields: flags.fields.clone(),
        }
    }

    pub fn is_structured(&self) -> bool {
        self.mode != Mode::Human
    }

    /// Emits one command's payload as `{"schema":"mailbox/v1","command":…,…}`.
    pub fn reply<T: Serialize>(&self, command: &str, payload: &T) -> std::io::Result<()> {
        let value =
            serde_json::to_value(Reply::new(command, payload)).unwrap_or(serde_json::Value::Null);
        let value = self.project(value);
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "{}",
            serde_json::to_string(&value).unwrap_or_default()
        )
    }

    /// Emits a list as NDJSON: the envelope first, then one object per row, so
    /// a consumer can start work before the command finishes.
    pub fn ndjson_list<T: Serialize>(
        &self,
        command: &str,
        items: &[T],
        cursor: Option<&str>,
    ) -> std::io::Result<()> {
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "{}",
            serde_json::json!({
                "schema": SCHEMA_VERSION,
                "command": command,
                "type": "begin",
                "count": items.len(),
            })
        )?;
        for item in items {
            let value = self.project(serde_json::to_value(item).unwrap_or(serde_json::Value::Null));
            writeln!(
                stdout,
                "{}",
                serde_json::to_string(&value).unwrap_or_default()
            )?;
        }
        writeln!(
            stdout,
            "{}",
            serde_json::json!({ "type": "end", "cursor": cursor })
        )
    }

    /// Applies `--fields`. Applied to objects and to the objects inside a
    /// top-level array-valued key, which is what makes `--fields id,subject`
    /// work on `ls` without the caller naming the container.
    ///
    /// The envelope survives regardless: every success is stamped with its
    /// schema version (§6.4), and a projection that stripped it would produce
    /// a document a consumer could not version-check.
    fn project(&self, value: serde_json::Value) -> serde_json::Value {
        let Some(fields) = &self.fields else {
            return value;
        };
        match value {
            serde_json::Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (k, v) in map {
                    if let serde_json::Value::Array(items) = v {
                        out.insert(
                            k,
                            serde_json::Value::Array(
                                items.into_iter().map(|i| pick(i, fields)).collect(),
                            ),
                        );
                    } else if ENVELOPE_KEYS.contains(&k.as_str()) || fields.iter().any(|f| f == &k)
                    {
                        out.insert(k, v);
                    }
                }
                serde_json::Value::Object(out)
            }
            other => pick(other, fields),
        }
    }

    pub fn print(&self, s: impl AsRef<str>) {
        if self.quiet {
            return;
        }
        println!("{}", s.as_ref());
    }

    /// A note to the user that is not part of the output — suppressed entirely
    /// in structured mode, where anything on stdout would corrupt the document.
    pub fn note(&self, s: impl AsRef<str>) {
        if self.quiet || self.is_structured() {
            return;
        }
        eprintln!("{}", self.dim(s.as_ref()));
    }

    /// A warning that belongs on stderr in every mode (§6.4 warns agents that
    /// use index references, and that warning must survive `--json`).
    pub fn warn(&self, s: impl AsRef<str>) {
        if self.quiet {
            return;
        }
        eprintln!("{}", self.paint(s.as_ref(), YELLOW));
    }

    pub fn error(&self, err: &ErrorResponse) {
        if self.is_structured() {
            eprintln!("{}", serde_json::to_string(err).unwrap_or_default());
            return;
        }
        eprintln!("{}: {}", self.paint("error", RED), err.error.message);
        if let Some(hint) = &err.error.hint {
            eprintln!("  {}", self.dim(hint));
        }
    }

    pub fn paint(&self, s: &str, code: &str) -> String {
        if !self.color {
            return s.to_string();
        }
        format!("\x1b[{code}m{s}\x1b[0m")
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint(s, DIM)
    }

    pub fn bold(&self, s: &str) -> String {
        self.paint(s, BOLD)
    }
}

fn pick(value: serde_json::Value, fields: &[String]) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .filter(|(k, _)| fields.iter().any(|f| f == k))
                .collect(),
        ),
        other => other,
    }
}

/// Envelope keys `--fields` never removes.
const ENVELOPE_KEYS: &[&str] = &["schema", "command", "cursor", "total"];

pub const RED: &str = "31";
pub const GREEN: &str = "32";
pub const YELLOW: &str = "33";
pub const BLUE: &str = "34";
pub const CYAN: &str = "36";
pub const DIM: &str = "2";
pub const BOLD: &str = "1";

/// §6.3: terminal-width-aware. `COLUMNS` wins so a test can pin the width; a
/// non-TTY gets a fixed width so redirected output is reproducible.
fn terminal_width(is_tty: bool) -> usize {
    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(n) = cols.parse::<usize>() {
            if n > 20 {
                return n;
            }
        }
    }
    if !is_tty {
        return 100;
    }
    terminal_size::terminal_size()
        .map(|(w, _)| w.0 as usize)
        .unwrap_or(100)
}

/// Pipes long output through a pager, but only when stdout is a TTY and the
/// output would not fit (§6.3).
///
/// Never in structured mode: a pager between an agent and its JSON is a hang.
pub fn maybe_page(out: &Out, text: &str) {
    let is_tty = std::io::stdout().is_terminal();
    let lines = text.lines().count();
    let height = terminal_size::terminal_size()
        .map(|(_, h)| h.0 as usize)
        .unwrap_or(24);

    if out.is_structured() || !is_tty || lines < height.saturating_sub(1) {
        println!("{text}");
        return;
    }

    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_string());
    // -F quits if it fits, -R keeps our colours, -X leaves the output on screen.
    let args: Vec<&str> = if pager == "less" {
        vec!["-FRX"]
    } else {
        vec![]
    };

    match std::process::Command::new(&pager)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
        // No pager on the box is not an error worth failing a command over.
        Err(_) => println!("{text}"),
    }
}

//! Aligned, width-aware tables for human output (§6.3).
//!
//! Small and hand-rolled because the requirement is narrow: columns that align,
//! one flexible column that absorbs the remaining width, and no ANSI in the
//! measurements. A table library would bring styling we do not want and a
//! dependency we would have to keep out of the mobile build.

use crate::output::Out;

pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
}

pub struct Column {
    pub header: String,
    /// The column that gives up its width first when the terminal is narrow.
    /// Exactly one column should be flexible; the last one wins if several are.
    pub flexible: bool,
    pub align_right: bool,
    /// Never render narrower than this, even when the terminal is narrow.
    pub min_width: usize,
}

impl Column {
    pub fn new(header: &str) -> Column {
        Column {
            header: header.to_string(),
            flexible: false,
            align_right: false,
            min_width: 0,
        }
    }

    pub fn flexible(mut self) -> Column {
        self.flexible = true;
        self
    }

    pub fn right(mut self) -> Column {
        self.align_right = true;
        self
    }

    pub fn min(mut self, n: usize) -> Column {
        self.min_width = n;
        self
    }
}

impl Table {
    pub fn new(columns: Vec<Column>) -> Table {
        Table {
            columns,
            rows: Vec::new(),
        }
    }

    pub fn push(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Renders to a string, fitting `out.width`.
    pub fn render(&self, out: &Out) -> String {
        let gap = 2;
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let content = self
                    .rows
                    .iter()
                    .map(|r| r.get(i).map(|s| display_width(s)).unwrap_or(0))
                    .max()
                    .unwrap_or(0);
                content.max(display_width(&c.header)).max(c.min_width)
            })
            .collect();

        // Shrink the flexible column until the table fits. Truncating there is
        // better than wrapping: a subject that spills onto a second line makes
        // a listing of fifty messages unreadable.
        let total: usize = widths.iter().sum::<usize>() + gap * widths.len().saturating_sub(1);
        if total > out.width {
            if let Some(flex) = self.columns.iter().rposition(|c| c.flexible) {
                let excess = total - out.width;
                let floor = self.columns[flex].min_width.max(8);
                widths[flex] = widths[flex].saturating_sub(excess).max(floor);
            }
        }

        let mut s = String::new();
        if !self.columns.iter().all(|c| c.header.is_empty()) {
            let header: Vec<String> = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| pad(&c.header, widths[i], c.align_right))
                .collect();
            s.push_str(&out.dim(header.join(&" ".repeat(gap)).trim_end()));
            s.push('\n');
        }
        for row in &self.rows {
            let cells: Vec<String> = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let cell = row.get(i).map(String::as_str).unwrap_or("");
                    pad(&truncate(cell, widths[i]), widths[i], c.align_right)
                })
                .collect();
            s.push_str(cells.join(&" ".repeat(gap)).trim_end());
            s.push('\n');
        }
        s.trim_end().to_string()
    }
}

/// Width in terminal columns, ignoring ANSI escapes.
///
/// Not full Unicode width: CJK and emoji count as one here, which misaligns
/// those rows by a column or two. Correcting it means a width table, and the
/// cost of that dependency in a static binary (§10's 25 MB budget) is not worth
/// paying for alignment that is off rather than wrong.
pub fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c == 'm' {
                in_escape = false;
            }
            continue;
        }
        if c == '\x1b' {
            in_escape = true;
            continue;
        }
        width += 1;
    }
    width
}

/// Truncates to `max` columns, marking the cut with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    for (width, c) in s.chars().enumerate() {
        if width + 1 >= max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

fn pad(s: &str, width: usize, right: bool) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    let fill = " ".repeat(width - w);
    if right {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }
}

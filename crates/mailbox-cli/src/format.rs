//! Human rendering (§6.3): colored, aligned, terminal-width-aware.
//!
//! Everything here is presentation. Nothing in this file decides *what* is
//! shown — that is the core's job — so a change here can never change what the
//! `--json` contract reports.

use mailbox_api::{Address, MessageDetail, MessageSummary, Thread};

use crate::output::{Out, BLUE, BOLD, CYAN, DIM, GREEN, MAGENTA, YELLOW};
use crate::table::{Column, Table};

/// One row per message: flags, date, sender, subject.
///
/// An account column appears only when the listing actually spans more than
/// one (§7: "multi-account is invisible until you want it"). Self-configuring
/// rather than flag-driven, so a single-account mailbox never pays for it.
pub fn message_table(out: &Out, messages: &[MessageSummary], full_ids: bool) -> String {
    let accounts: std::collections::BTreeSet<&str> =
        messages.iter().map(|m| m.account.as_str()).collect();
    let show_account = accounts.len() > 1;

    let mut columns = vec![
        Column::new("").min(2),
        Column::new("id").min(if full_ids { 64 } else { 7 }),
    ];
    if show_account {
        columns.push(Column::new("acct").min(4));
    }
    columns.extend([
        Column::new("date").min(11),
        Column::new("from").min(14),
        Column::new("subject").flexible().min(20),
    ]);
    let mut table = Table::new(columns);

    for m in messages {
        let unread = !m.flags.iter().any(|f| f == "seen");
        let subject = if m.subject.is_empty() {
            "(no subject)"
        } else {
            &m.subject
        };
        let subject = if unread {
            out.paint(subject, BOLD)
        } else {
            subject.to_string()
        };
        let mut row = vec![
            marks(out, m),
            out.dim(if full_ids { &m.id } else { short(&m.id) }),
        ];
        if show_account {
            row.push(out.paint(&m.account, account_color(&m.account)));
        }
        row.extend([
            out.paint(&relative_date(&m.date), DIM),
            out.paint(&truncate_addr(&m.from), CYAN),
            subject,
        ]);
        table.push(row);
    }
    table.render(out)
}

/// §7's "per-message account color": a stable colour per account name.
///
/// Derived from the name rather than from the account's position in the config,
/// so an account keeps its colour when another is added or removed — the point
/// of a colour is that it is recognisable, and one that shifts is worse than
/// none.
fn account_color(account: &str) -> &'static str {
    const PALETTE: &[&str] = &[CYAN, GREEN, YELLOW, MAGENTA, BLUE];
    let hash = account.bytes().fold(0u32, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u32::from(b))
    });
    PALETTE[hash as usize % PALETTE.len()]
}

/// The flag column: unread, flagged, attachment. Three characters, because a
/// listing is read at a glance and a wider column costs subject width.
fn marks(out: &Out, m: &MessageSummary) -> String {
    let unread = !m.flags.iter().any(|f| f == "seen");
    let flagged = m.flags.iter().any(|f| f == "flagged");
    let mut s = String::new();
    s.push_str(&if unread {
        out.paint("●", BLUE)
    } else {
        " ".into()
    });
    s.push_str(&if flagged {
        out.paint("★", YELLOW)
    } else {
        " ".into()
    });
    s.push_str(&if m.has_attachment {
        out.dim("@")
    } else {
        " ".into()
    });
    s
}

pub fn thread_summary(out: &Out, threads: &[Thread]) -> String {
    let mut table = Table::new(vec![
        Column::new("id").min(7),
        Column::new("n").right().min(3),
        Column::new("last").min(11),
        Column::new("participants").min(16),
        Column::new("subject").flexible().min(20),
    ]);
    for t in threads {
        let subject = if t.subject.is_empty() {
            "(no subject)"
        } else {
            &t.subject
        };
        let subject = if t.unread_count > 0 {
            out.bold(subject)
        } else {
            subject.to_string()
        };
        table.push(vec![
            out.dim(short(&t.id)),
            if t.unread_count > 0 {
                out.paint(&format!("{}/{}", t.unread_count, t.message_count), BLUE)
            } else {
                t.message_count.to_string()
            },
            out.dim(&relative_date(&t.last_date)),
            out.paint(&truncate_addr(&t.participants), CYAN),
            subject,
        ]);
    }
    table.render(out)
}

/// A whole message: headers, then the body.
pub fn message_detail(out: &Out, m: &MessageDetail, show_attachments: bool) -> String {
    let mut s = String::new();
    let h = |name: &str, value: &str| -> String {
        format!("{} {}\n", out.dim(&format!("{name:>9}:")), value)
    };

    s.push_str(&h("From", &addresses(&m.summary.from)));
    if !m.summary.to.is_empty() {
        s.push_str(&h("To", &addresses(&m.summary.to)));
    }
    if !m.summary.cc.is_empty() {
        s.push_str(&h("Cc", &addresses(&m.summary.cc)));
    }
    s.push_str(&h("Date", &m.summary.date));
    s.push_str(&h("Subject", &out.bold(&m.summary.subject)));
    s.push_str(&h("Folder", &m.summary.folder));
    if !m.summary.flags.is_empty() {
        s.push_str(&h("Flags", &m.summary.flags.join(" ")));
    }
    s.push_str(&h("Id", &out.dim(&m.summary.id)));

    if show_attachments && !m.attachments.is_empty() {
        s.push('\n');
        for a in &m.attachments {
            if !a.is_attachment {
                continue;
            }
            s.push_str(&format!(
                "  {} {}  {}  {}\n",
                out.paint("@", GREEN),
                a.filename.clone().unwrap_or_else(|| "(unnamed)".into()),
                out.dim(&a.mime_type),
                out.dim(&human_size(a.size)),
            ));
        }
    }

    s.push('\n');
    match &m.body {
        Some(body) => {
            s.push_str(&body.content);
            if body.truncated {
                s.push_str(&format!("\n\n{}", out.dim("[truncated]")));
            }
        }
        // Not an error: §5.1 fetches bodies lazily, so this is the normal state
        // for older mail until it is opened or `sync --full` runs.
        None => s.push_str(&out.dim("[body not downloaded — run `mailbox sync` to fetch it]")),
    }
    s
}

/// A thread as an indented tree.
pub fn thread_detail(out: &Out, t: &Thread) -> String {
    let mut s = format!("{}\n", out.bold(&t.subject));
    s.push_str(&out.dim(&format!(
        "{} messages, {} unread, last {}\n\n",
        t.message_count, t.unread_count, t.last_date
    )));
    for m in &t.messages {
        let unread = !m.flags.iter().any(|f| f == "seen");
        let dot = if unread {
            out.paint("●", BLUE)
        } else {
            out.dim("·")
        };
        s.push_str(&format!(
            "{} {} {}  {}\n",
            dot,
            out.dim(short(&m.id)),
            out.paint(&truncate_addr(&m.from), CYAN),
            out.dim(&relative_date(&m.date)),
        ));
        if let Some(p) = &m.preview {
            s.push_str(&format!(
                "    {}\n",
                out.dim(&crate::table::truncate(p, out.width.saturating_sub(6)))
            ));
        }
    }
    s
}

fn addresses(list: &[Address]) -> String {
    list.iter()
        .map(|a| match &a.name {
            Some(n) => format!("{n} <{}>", a.address),
            None => a.address.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The shortest useful rendering of a sender: the display name if there is one,
/// the local part if not.
fn truncate_addr(list: &[Address]) -> String {
    let Some(first) = list.first() else {
        return String::new();
    };
    let base = match &first.name {
        Some(n) if !n.is_empty() => n.clone(),
        _ => first
            .address
            .split('@')
            .next()
            .unwrap_or(&first.address)
            .to_string(),
    };
    if list.len() > 1 {
        format!("{base} +{}", list.len() - 1)
    } else {
        base
    }
}

pub fn short(id: &str) -> &str {
    &id[..7.min(id.len())]
}

/// Time today, month-day this year, ISO date otherwise — the format every mail
/// client converged on, because it puts the distinguishing part first.
pub fn relative_date(rfc3339: &str) -> String {
    let Ok(dt) =
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
    else {
        return rfc3339.to_string();
    };
    let local = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let dt = dt.to_offset(local);
    let now = time::OffsetDateTime::now_utc().to_offset(local);

    if dt.date() == now.date() {
        let fmt = time::macros::format_description!("[hour]:[minute]");
        return dt.format(fmt).unwrap_or_default();
    }
    if dt.year() == now.year() {
        let fmt = time::macros::format_description!("[month repr:short] [day]");
        return dt.format(fmt).unwrap_or_default();
    }
    let fmt = time::macros::format_description!("[year]-[month]-[day]");
    dt.format(fmt).unwrap_or_default()
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

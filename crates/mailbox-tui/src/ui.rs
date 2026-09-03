//! Drawing (§7).
//!
//! Reads [`App`] and produces frames. Holds no state of its own, so what is on
//! screen is always a function of what the state machine decided — there is no
//! second copy of the truth to drift.

use mailbox_api::MessageSummary;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Prompt, View};
use crate::keys::{Action, Keymap};

/// Colours, in one place.
///
/// Only the terminal's own palette — no fixed RGB — so the interface follows
/// whatever theme the user has rather than fighting it.
mod theme {
    use ratatui::style::Color;
    pub const UNREAD: Color = Color::Blue;
    pub const FLAGGED: Color = Color::Yellow;
    pub const SENDER: Color = Color::Cyan;
    pub const DIM: Color = Color::DarkGray;
    pub const ACCENT: Color = Color::Green;
    pub const WARN: Color = Color::Yellow;
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App, keymap: &Keymap) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // the content
            Constraint::Length(1), // status
            Constraint::Length(1), // prompt
        ])
        .split(frame.area());

    // The renderer owns the real page height, so paging moves by what the user
    // can actually see rather than by a guess.
    app.page_size = (areas[0].height as usize).saturating_sub(2).max(1);

    match app.view {
        View::List => draw_list(frame, areas[0], app),
        View::Message => draw_message(frame, areas[0], app),
        View::Thread => draw_thread(frame, areas[0], app),
        View::Links => draw_links(frame, areas[0], app),
        View::Help => draw_help(frame, areas[0], keymap),
    }
    draw_status(frame, areas[1], app);
    draw_prompt(frame, areas[2], app);
}

fn draw_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if app.messages.is_empty() {
        let text = if app.query.is_empty() {
            "no messages — press @ to sync, ? for help"
        } else {
            "nothing matches"
        };
        frame.render_widget(
            Paragraph::new(text).style(Style::default().fg(theme::DIM)),
            area,
        );
        return;
    }

    let show_account = app
        .messages
        .iter()
        .map(|m| m.account.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        > 1;

    let items: Vec<ListItem> = app
        .messages
        .iter()
        .map(|m| ListItem::new(row(m, show_account)))
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected));

    frame.render_stateful_widget(
        List::new(items).highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        ),
        area,
        &mut state,
    );
}

/// One line of the index.
fn row(m: &MessageSummary, show_account: bool) -> Line<'static> {
    let unread = !m.flags.iter().any(|f| f == "seen");
    let flagged = m.flags.iter().any(|f| f == "flagged");

    let mut spans = vec![
        Span::styled(
            if unread { "●" } else { " " },
            Style::default().fg(theme::UNREAD),
        ),
        Span::styled(
            if flagged { "★" } else { " " },
            Style::default().fg(theme::FLAGGED),
        ),
        Span::styled(
            if m.has_attachment { "@ " } else { "  " },
            Style::default().fg(theme::DIM),
        ),
        Span::styled(
            format!("{:<12}", short_date(&m.date)),
            Style::default().fg(theme::DIM),
        ),
    ];

    if show_account {
        spans.push(Span::styled(
            format!("{:<8}", truncate(&m.account, 7)),
            Style::default().fg(theme::ACCENT),
        ));
    }

    spans.push(Span::styled(
        format!("{:<20}", truncate(&sender(m), 19)),
        Style::default().fg(theme::SENDER),
    ));

    let subject = if m.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        m.subject.clone()
    };
    spans.push(Span::styled(
        subject,
        if unread {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        },
    ));

    Line::from(spans)
}

fn draw_message(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(detail) = &app.open else {
        frame.render_widget(
            Paragraph::new("loading…").style(Style::default().fg(theme::DIM)),
            area,
        );
        return;
    };

    let mut lines = vec![
        header("From", &addresses(&detail.summary.from)),
        header("To", &addresses(&detail.summary.to)),
    ];
    if !detail.summary.cc.is_empty() {
        lines.push(header("Cc", &addresses(&detail.summary.cc)));
    }
    lines.push(header("Date", &detail.summary.date));
    lines.push(Line::from(vec![
        Span::styled("  Subject: ", Style::default().fg(theme::DIM)),
        Span::styled(
            detail.summary.subject.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    if !detail.attachments.is_empty() {
        let names: Vec<String> = detail
            .attachments
            .iter()
            .filter(|a| a.is_attachment)
            .map(|a| a.filename.clone().unwrap_or_else(|| a.part.clone()))
            .collect();
        if !names.is_empty() {
            lines.push(header("Attach", &names.join(", ")));
        }
    }
    lines.push(Line::from(""));

    match &detail.body {
        Some(body) => {
            for line in body.content.lines() {
                // Quoted text is dimmed, so a reply's own words stand out from
                // what it is replying to.
                let style = if line.trim_start().starts_with('>') {
                    Style::default().fg(theme::DIM)
                } else {
                    Style::default()
                };
                lines.push(Line::from(Span::styled(line.to_string(), style)));
            }
        }
        None => lines.push(Line::from(Span::styled(
            "[body not downloaded]",
            Style::default().fg(theme::DIM),
        ))),
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.body_scroll, 0)),
        area,
    );
}

/// The open message's links, one per line, numbered as the body numbers them.
///
/// The whole URL is shown, never a shortened or prettified form. A link in mail
/// is exactly the case where the displayed text and the destination disagree on
/// purpose, so the destination is what gets displayed here (§9).
fn draw_links(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let links = app.links();
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            "  links in this message — Enter to open, q to go back",
            Style::default().fg(theme::DIM),
        )),
        Line::from(""),
    ];

    for (i, url) in links.iter().enumerate() {
        let selected = i == app.link_selected;
        let marker = if selected { "> " } else { "  " };
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{marker}[{}] ", i + 1),
                Style::default().fg(theme::DIM),
            ),
            Span::styled(url.clone(), style),
        ]));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_thread(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(current) = app.current() else { return };
    let thread_id = current.thread_id.clone();

    let mut lines = Vec::new();
    for m in app.messages.iter().filter(|m| m.thread_id == thread_id) {
        let unread = !m.flags.iter().any(|f| f == "seen");
        lines.push(Line::from(vec![
            Span::styled(
                if unread { "● " } else { "· " },
                Style::default().fg(theme::UNREAD),
            ),
            Span::styled(
                format!("{:<20}", truncate(&sender(m), 19)),
                Style::default().fg(theme::SENDER),
            ),
            Span::styled(short_date(&m.date), Style::default().fg(theme::DIM)),
        ]));
        if let Some(preview) = &m.preview {
            lines.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    truncate(preview, area.width.saturating_sub(6) as usize)
                ),
                Style::default().fg(theme::DIM),
            )));
        }
    }

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM).title(" thread ")),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, keymap: &Keymap) {
    const ORDER: &[Action] = &[
        Action::Down,
        Action::Up,
        Action::PageDown,
        Action::PageUp,
        Action::Top,
        Action::Bottom,
        Action::Open,
        Action::Back,
        Action::Thread,
        Action::Search,
        Action::ToggleRead,
        Action::ToggleFlagged,
        Action::Tag,
        Action::Archive,
        Action::Delete,
        Action::Spam,
        Action::Reply,
        Action::ReplyAll,
        Action::Forward,
        Action::Compose,
        Action::Links,
        Action::Snooze,
        Action::Pipe,
        Action::Sync,
        Action::Help,
        Action::Quit,
    ];

    // Built from the live keymap, so a rebinding shows up in the help without
    // anyone remembering to update it.
    let lines: Vec<Line> = ORDER
        .iter()
        .map(|action| {
            Line::from(vec![
                Span::styled(
                    // Padded *and* separated: a key list exactly as wide as the
                    // column would otherwise run into the description.
                    format!("  {:<14}", keymap.keys_for(*action).join(" ")),
                    Style::default().fg(theme::ACCENT),
                ),
                Span::raw(format!(" {}", action.describe())),
            ])
        })
        .collect();

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" keys — any key to close "),
        ),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let unread = app
        .messages
        .iter()
        .filter(|m| !m.flags.iter().any(|f| f == "seen"))
        .count();

    let mut spans = vec![Span::styled(
        format!(" {} messages, {unread} unread ", app.messages.len()),
        Style::default().add_modifier(Modifier::REVERSED),
    )];

    if !app.query.is_empty() {
        spans.push(Span::styled(
            format!(" /{} ", app.query),
            Style::default().fg(theme::ACCENT),
        ));
    }
    if app.syncing {
        // Shown, never waited on: §7 forbids modal "please wait" states, so a
        // sync is a word in the status bar and nothing else.
        spans.push(Span::styled(" syncing… ", Style::default().fg(theme::WARN)));
    }
    if !app.status.is_empty() {
        spans.push(Span::raw(format!(" {}", app.status)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_prompt(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(prompt) = &app.prompt else {
        frame.render_widget(
            Paragraph::new(Span::styled(" ? for help", Style::default().fg(theme::DIM))),
            area,
        );
        return;
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt.label(), Style::default().fg(theme::ACCENT)),
            Span::raw(prompt.text().to_string()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ])),
        area,
    );
    let _ = Prompt::Search(String::new());
}

fn header(name: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {name:>7}: "), Style::default().fg(theme::DIM)),
        Span::raw(value.to_string()),
    ])
}

fn addresses(list: &[mailbox_api::Address]) -> String {
    list.iter()
        .map(|a| match &a.name {
            Some(n) if !n.is_empty() => format!("{n} <{}>", a.address),
            _ => a.address.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn sender(m: &MessageSummary) -> String {
    m.from
        .first()
        .map(|a| match &a.name {
            Some(n) if !n.trim().is_empty() => n.clone(),
            _ => a
                .address
                .split('@')
                .next()
                .unwrap_or(&a.address)
                .to_string(),
        })
        .unwrap_or_default()
}

/// `12:04` today, `Sep 01` this year, `2025-08-30` before that.
fn short_date(rfc3339: &str) -> String {
    let Ok(dt) =
        time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
    else {
        return rfc3339.chars().take(10).collect();
    };
    let now = time::OffsetDateTime::now_utc();
    if dt.date() == now.date() {
        let f = time::macros::format_description!("[hour]:[minute]");
        dt.format(f).unwrap_or_default()
    } else if dt.year() == now.year() {
        let f = time::macros::format_description!("[month repr:short] [day]");
        dt.format(f).unwrap_or_default()
    } else {
        let f = time::macros::format_description!("[year]-[month]-[day]");
        dt.format(f).unwrap_or_default()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Unused today; kept so the palette has a single home.
#[allow(dead_code)]
fn accent() -> Color {
    theme::ACCENT
}

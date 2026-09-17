//! Drawing (§7).
//!
//! Reads [`App`] and produces frames. Holds no state of its own, so what is on
//! screen is always a function of what the state machine decided — there is no
//! second copy of the truth to drift.

use mailbox_api::MessageSummary;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::{App, Prompt, View};
use crate::keys::{Action, Keymap};

/// Colours, in one place.
///
/// Only the terminal's own palette — no fixed RGB — so the interface follows
/// whatever theme the user has rather than fighting it.
use crate::theme;

/// Draws a frame, and reports where any links landed.
///
/// The link positions come back rather than being acted on here: turning them
/// into terminal hyperlinks means writing escape sequences straight at the
/// terminal after the frame is on screen, which is the shell's job (§4's thin
/// frontends) and cannot be done through a ratatui buffer at all.
pub fn draw(
    frame: &mut Frame<'_>,
    app: &mut App,
    keymap: &Keymap,
) -> Vec<crate::markdown::LinkSpan> {
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

    let mut links = Vec::new();
    match app.view {
        View::List => draw_list(frame, areas[0], app),
        View::Message => draw_message(frame, areas[0], app, &mut links),
        View::Thread => draw_thread(frame, areas[0], app),
        View::Links => draw_links(frame, areas[0], app, &mut links),
        View::Help => draw_help(frame, areas[0], keymap),
    }
    draw_status(frame, areas[1], app);
    draw_prompt(frame, areas[2], app);
    links
}

fn draw_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    // Before "no messages", because an unconfigured mailbox is empty for a
    // reason no amount of syncing will fix.
    if app.no_accounts {
        draw_setup(frame, area);
        return;
    }

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

/// The first launch (§9): what to type, and where.
///
/// The interface starts without an account rather than refusing to — a mail
/// client that will not open until it has mail leaves the instructions
/// nowhere to be read. The command is shown, not offered: adding an account
/// needs a password or a browser sign-in, which is a terminal's job and not
/// this screen's.
fn draw_setup(frame: &mut Frame<'_>, area: Rect) {
    let dim = Style::default().fg(theme::DIM);
    let command = Style::default()
        .fg(theme::ACCENT)
        .add_modifier(Modifier::BOLD);

    let lines = vec![
        Line::from(Span::styled(
            "No account yet.",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Add one in another terminal — the server settings are discovered",
            dim,
        )),
        Line::from(Span::styled("from the address:", dim)),
        Line::from(""),
        Line::from(Span::styled(
            "    mailbox accounts add you@example.com",
            command,
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Google, Microsoft, and Yahoo sign in through the browser: add",
            dim,
        )),
        Line::from(Span::styled("--oauth --login.", dim)),
        Line::from(""),
        Line::from(Span::styled(
            "Already set up on another device? `mailbox pair` copies its",
            dim,
        )),
        Line::from(Span::styled("accounts across.", dim)),
        Line::from(""),
        Line::from(Span::styled(
            "This screen is watching, and opens your mail as soon as an",
            dim,
        )),
        Line::from(Span::styled(
            "account appears.  ? for help, q to quit.",
            dim,
        )),
    ];

    // Indented by one, so the text does not start against the terminal's edge.
    // Left alone otherwise: a centred block moves every time the window is
    // resized, and there is nothing here worth that.
    let inner = Rect {
        x: area.x.saturating_add(2),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(1),
    };
    frame.render_widget(Paragraph::new(lines), inner);
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

fn draw_message(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    found: &mut Vec<crate::markdown::LinkSpan>,
) {
    use crate::markdown::{Body, BodyLine, Piece};

    let Some(detail) = &app.open else {
        frame.render_widget(
            Paragraph::new("loading…").style(Style::default().fg(theme::DIM)),
            area,
        );
        return;
    };

    // The headers are part of the same scrolled flow as the body, as they were
    // when both went through one `Paragraph`: scrolling a long message should
    // move them off rather than spend six rows of a small screen on them.
    let mut lines = vec![
        header("From", &addresses(&detail.summary.from)),
        header("To", &addresses(&detail.summary.to)),
    ];
    if !detail.summary.cc.is_empty() {
        lines.push(header("Cc", &addresses(&detail.summary.cc)));
    }
    lines.push(header("Date", &detail.summary.date));
    lines.push(BodyLine::plain(vec![
        Piece::new("  Subject: ", Style::default().fg(theme::DIM)),
        Piece::new(
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
    lines.push(BodyLine::plain(vec![Piece::new("", Style::default())]));

    let empty: Vec<String> = Vec::new();
    let links = match &detail.body {
        Some(body) => {
            lines.extend(crate::markdown::body_lines(
                &body.content,
                area.width as usize,
                &body.links,
            ));
            if body.truncated {
                lines.push(BodyLine::plain(vec![Piece::new(
                    "[truncated]",
                    Style::default().fg(theme::DIM),
                )]));
            }
            &body.links
        }
        None => {
            lines.push(BodyLine::plain(vec![Piece::new(
                "[body not downloaded]",
                Style::default().fg(theme::DIM),
            )]));
            &empty
        }
    };

    frame.render_widget(
        Body {
            lines: &lines,
            links,
            scroll: app.body_scroll,
            found: Some(found),
        },
        area,
    );
}

/// The open message's links, one per line, numbered as the body numbers them.
///
/// The whole URL is shown, never a shortened or prettified form. A link in mail
/// is exactly the case where the displayed text and the destination disagree on
/// purpose, so the destination is what gets displayed here (§9). Each is a
/// terminal hyperlink too, so a right-click offers to copy it.
fn draw_links(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    found: &mut Vec<crate::markdown::LinkSpan>,
) {
    use crate::markdown::{Body, BodyLine, Piece};

    let links = app.links();
    let mut lines = vec![
        BodyLine::plain(vec![Piece::new(
            "  links in this message — Enter to open, y to copy, q to go back",
            Style::default().fg(theme::DIM),
        )]),
        BodyLine::plain(vec![Piece::new("", Style::default())]),
    ];

    for (i, url) in links.iter().enumerate() {
        let selected = i == app.link_selected;
        let marker = if selected { "> " } else { "  " };
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(BodyLine {
            lead: Piece::new(
                format!("{marker}[{}] ", i + 1),
                Style::default().fg(theme::DIM),
            ),
            hang: Piece::new(" ".repeat(6), Style::default()),
            pieces: vec![Piece {
                text: url.clone(),
                style,
                link: Some(i),
            }],
        });
    }

    frame.render_widget(
        Body {
            lines: &lines,
            links,
            scroll: 0,
            found: Some(found),
        },
        area,
    );
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
        Action::Undo,
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
        Action::Yank,
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

    // "0 messages, 0 unread" over the setup guide is a count of something
    // that does not exist yet, and reads as a mailbox that has failed to load
    // rather than one that has not been set up.
    let summary = if app.no_accounts {
        " no account ".to_string()
    } else {
        format!(" {} messages, {unread} unread ", app.messages.len())
    };

    let mut spans = vec![Span::styled(
        summary,
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

fn header(name: &str, value: &str) -> crate::markdown::BodyLine {
    use crate::markdown::{BodyLine, Piece};
    BodyLine {
        // A wrapped recipient list lines up under the first name rather than
        // under the field label.
        lead: Piece::new(format!("  {name:>7}: "), Style::default().fg(theme::DIM)),
        hang: Piece::new(" ".repeat(11), Style::default()),
        pieces: vec![Piece::new(value.to_string(), Style::default())],
    }
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

#[cfg(test)]
mod tests {
    use super::draw;
    use crate::app::App;
    use crate::keys::Keymap;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn painted(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("a test terminal");
        terminal
            .draw(|frame| {
                draw(frame, app, &Keymap::vim());
            })
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// The first screen a new install sees.
    ///
    /// "no messages" would be true and useless: it sends someone looking for
    /// mail that has nowhere to arrive from, and offers a sync that cannot
    /// help. The index has to say what is actually missing.
    #[test]
    fn an_unconfigured_mailbox_says_how_to_add_an_account() {
        let mut app = App::new();
        app.no_accounts = true;

        let screen = painted(&mut app);
        assert!(
            screen.contains("mailbox accounts add"),
            "the setup guide does not name the command: {screen:?}"
        );
        assert!(
            !screen.contains("no messages"),
            "an unconfigured mailbox was reported as an empty one: {screen:?}"
        );
    }

    /// The other half of the same rule: a mailbox that *is* configured and
    /// happens to be empty still wants the sync it can act on.
    #[test]
    fn a_configured_but_empty_mailbox_still_offers_a_sync() {
        let mut app = App::new();

        let screen = painted(&mut app);
        assert!(
            screen.contains("no messages"),
            "an empty inbox lost its own message: {screen:?}"
        );
        assert!(!screen.contains("mailbox accounts add"));
    }
}

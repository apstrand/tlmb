//! Drawing the interface (§7).
//!
//! > **GUI** — Spark-like: unified inbox, thread cards, a single search field
//! > that takes the §6.2 grammar, swipe/keyboard triage (archive, snooze, tag),
//! > and a compose window whose *From* selector is one keystroke away.
//! > Multi-account is invisible until you want it: one list, per-message
//! > account colour, `account:` to split.
//!
//! This module holds no state of its own. It reads [`App`] and turns keystrokes
//! and clicks into calls on it, so what the interface *does* stays testable
//! without a window and this file stays a description of what it looks like.
//!
//! The body is drawn from the flattened text rendering rather than the styled
//! Markdown the terminal shows. That is a deliberate stop short of the TUI:
//! the inline Markdown parser lives in `mailbox-tui`, and a second copy here
//! would be two sets of subtle rules about what counts as emphasis. Sharing it
//! properly means extracting it into a crate both can use, which is worth doing
//! and is not worth doing badly.

use egui::{Color32, Context, Key, RichText, ScrollArea, TextStyle};
use mailbox_api::MessageSummary;

use crate::app::{App, Focus};
use crate::backend::Backend;

/// Colours, named by role. Deliberately few: the terminal palette's discipline
/// applies here too.
mod theme {
    use egui::Color32;
    pub const UNREAD: Color32 = Color32::from_rgb(0x6c, 0xa9, 0xff);
    pub const FLAGGED: Color32 = Color32::from_rgb(0xe8, 0xb3, 0x39);
    pub const DIM: Color32 = Color32::from_rgb(0x8a, 0x8a, 0x8a);
    pub const ERROR: Color32 = Color32::from_rgb(0xe0, 0x6c, 0x6c);
    pub const LINK: Color32 = Color32::from_rgb(0x6c, 0xa9, 0xff);
}

/// Draws one frame.
pub fn draw(ctx: &Context, app: &mut App, backend: &mut dyn Backend) {
    // A panel remembers the width it was first laid out at — egui reloads it
    // from `PanelState` on every later frame and only falls back to
    // `default_width` when there is none. In a browser the first frame arrives
    // before the canvas has a size, so that first width is zero, and the panes
    // stay collapsed for the life of the page however large the window is.
    //
    // Drawing nothing until there is somewhere to draw is the whole fix. A
    // window this small has nothing to show in it anyway.
    let screen = ctx.screen_rect();
    if screen.width() < 2.0 || screen.height() < 2.0 {
        ctx.request_repaint();
        return;
    }

    keyboard(ctx, app, backend);

    egui::TopBottomPanel::top("search").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            // One field, and it takes the whole §6.2 grammar — `from:alice`,
            // `is:unread`, `has:attachment` — because there is one grammar
            // everywhere (§6.2) and a GUI with its own filter widgets would be
            // a second one.
            let field = ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text("search — from:alice is:unread has:attachment")
                    .desired_width(ui.available_width() - 160.0),
            );
            if app.focus == Focus::Search && !field.has_focus() {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                app.submit_search(backend);
                app.focus = Focus::List;
            }

            if ui.button("Sync").clicked() {
                app.sync(backend);
            }
            if app.in_flight > 0 {
                ui.spinner();
            }
            ui.add_space(4.0);
        });
    });

    egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            match &app.error {
                Some(error) => {
                    ui.label(RichText::new(error).color(theme::ERROR));
                }
                None => {
                    ui.label(RichText::new(&app.status).color(theme::DIM));
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(backend.label()).color(theme::DIM).small());
            });
        });
    });

    // Proportional, with floors. A fixed 540 px of panels is most of a phone
    // and a sliver of a desktop, and the first width is the one that sticks.
    let folder_width = (screen.width() * 0.15).clamp(140.0, 260.0);
    let list_width = (screen.width() * 0.28).clamp(240.0, 460.0);

    egui::SidePanel::left("folders")
        .resizable(true)
        .default_width(folder_width)
        .width_range(120.0..=320.0)
        .show(ctx, |ui| folders(ui, app, backend));

    egui::SidePanel::left("messages")
        .resizable(true)
        .default_width(list_width)
        .width_range(200.0..=640.0)
        .show(ctx, |ui| list(ui, app, backend));

    egui::CentralPanel::default().show(ctx, |ui| reading(ui, app, backend));
}

/// §7: multi-account is invisible until you want it. So this is a list of
/// folders across accounts, and clicking one writes the query that selects it
/// rather than putting the interface into a mode.
fn folders(ui: &mut egui::Ui, app: &mut App, backend: &mut dyn Backend) {
    ui.add_space(6.0);
    if ui
        .selectable_label(app.query.is_empty(), RichText::new("Inbox").strong())
        .clicked()
    {
        app.clear_search(backend);
    }
    ui.separator();

    ScrollArea::vertical()
        // Without this the pane collapses to the width of its longest folder
        // name, and the panel collapses with it.
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let folders = app.folders.clone();
            for folder in &folders {
                let query = format!("in:{}", quote_if_needed(&folder.path));
                let label = if folder.unread > 0 {
                    RichText::new(format!("{} ({})", folder.name, folder.unread)).strong()
                } else {
                    RichText::new(&folder.name)
                };
                if ui.selectable_label(app.query == query, label).clicked() {
                    app.search = query;
                    app.submit_search(backend);
                }
            }
        });
}

fn quote_if_needed(path: &str) -> String {
    if path.contains(' ') {
        format!("\"{path}\"")
    } else {
        path.to_string()
    }
}

/// The index: one row per message, unread and flagged visible at a glance.
fn list(ui: &mut egui::Ui, app: &mut App, backend: &mut dyn Backend) {
    ui.add_space(4.0);
    if app.messages.is_empty() {
        ui.label(RichText::new("no messages").color(theme::DIM));
        return;
    }

    let selected = app.selected;
    let mut clicked = None;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (index, message) in app.messages.iter().enumerate() {
                let response = row(ui, message, selected == Some(index));
                if response.clicked() {
                    clicked = Some(index);
                }
                if selected == Some(index) {
                    response.scroll_to_me(None);
                }
            }
        });
    if let Some(index) = clicked {
        app.select(index, backend);
        app.focus = Focus::List;
    }
}

fn row(ui: &mut egui::Ui, message: &MessageSummary, selected: bool) -> egui::Response {
    let unread = !message.flags.iter().any(|f| f == "seen");
    let flagged = message.flags.iter().any(|f| f == "flagged");

    let response = ui
        .push_id(&message.id, |ui| {
            egui::Frame::none()
                .fill(if selected {
                    ui.visuals().selection.bg_fill
                } else {
                    Color32::TRANSPARENT
                })
                .inner_margin(egui::Margin::symmetric(6.0, 4.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            let sender = sender_of(message);
                            let name = if unread {
                                RichText::new(sender).strong().color(theme::UNREAD)
                            } else {
                                RichText::new(sender)
                            };
                            ui.label(name);
                            if flagged {
                                ui.label(RichText::new("★").color(theme::FLAGGED));
                            }
                            if message.has_attachment {
                                ui.label(RichText::new("@").color(theme::DIM));
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(short_date(&message.date))
                                            .color(theme::DIM)
                                            .small(),
                                    );
                                },
                            );
                        });
                        let subject = if unread {
                            RichText::new(&message.subject).strong()
                        } else {
                            RichText::new(&message.subject)
                        };
                        ui.label(subject);
                        if let Some(preview) = &message.preview {
                            ui.label(RichText::new(preview).color(theme::DIM).small());
                        }
                    });
                });
        })
        .response;

    response.interact(egui::Sense::click())
}

/// The reading pane.
fn reading(ui: &mut egui::Ui, app: &mut App, backend: &mut dyn Backend) {
    let Some(detail) = app.open.clone() else {
        ui.centered_and_justified(|ui| {
            let text = if app.selected.is_some() {
                "loading…"
            } else {
                "no message selected"
            };
            ui.label(RichText::new(text).color(theme::DIM));
        });
        return;
    };

    ui.add_space(4.0);
    ui.heading(&detail.summary.subject);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(addresses(&detail.summary.from)).color(theme::DIM));
        ui.label(RichText::new("→").color(theme::DIM));
        ui.label(RichText::new(addresses(&detail.summary.to)).color(theme::DIM));
        ui.label(
            RichText::new(&detail.summary.date)
                .color(theme::DIM)
                .small(),
        );
    });

    ui.horizontal(|ui| {
        if ui.button("Archive").clicked() {
            app.move_to("archive", backend);
        }
        if ui.button("Delete").clicked() {
            app.move_to("trash", backend);
        }
        if ui.button("Spam").clicked() {
            app.move_to("junk", backend);
        }
        ui.separator();
        // §7's snooze, in its local-only form. The times are the ones anyone
        // actually picks; anything else goes through the CLI's grammar.
        for (label, until) in [("2h", "2h"), ("Tomorrow", "tomorrow"), ("Next week", "+7d")] {
            if ui.button(label).clicked() {
                app.snooze(until, backend);
            }
        }
    });
    ui.separator();

    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !detail.attachments.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    for attachment in detail.attachments.iter().filter(|a| a.is_attachment) {
                        let name = attachment
                            .filename
                            .clone()
                            .unwrap_or_else(|| attachment.part.clone());
                        ui.label(RichText::new(format!("@ {name}")).color(theme::DIM));
                    }
                });
                ui.separator();
            }

            match &detail.body {
                Some(body) => {
                    for line in body.content.lines() {
                        // The flattened body ends with its own numbered list of
                        // destinations (§6.4). Those lines become links rather than
                        // being repeated underneath as a second list — the reader
                        // should see each URL once, and be able to follow it.
                        if let Some((number, url)) = footnote(line) {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.label(RichText::new(number).color(theme::DIM));
                                // §9: the destination is what is displayed, and it
                                // is followed only when someone clicks it.
                                ui.hyperlink_to(RichText::new(url).color(theme::LINK), url);
                            });
                            continue;
                        }
                        // Quoted text is dimmed so a reply's own words stand out
                        // from what it is replying to — the same rule as the TUI's.
                        let text = if line.trim_start().starts_with('>') {
                            RichText::new(line).color(theme::DIM)
                        } else {
                            RichText::new(line)
                        };
                        ui.label(text.text_style(TextStyle::Body));
                    }
                }
                None => {
                    ui.label(RichText::new("[body not downloaded]").color(theme::DIM));
                }
            }
        });
}

/// §7 asks the GUI to share the terminal's muscle memory where it can.
fn keyboard(ctx: &Context, app: &mut App, backend: &mut dyn Backend) {
    // While the search field has focus every key is text, or `/` could never
    // be searched for.
    if app.focus == Focus::Search {
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            app.focus = Focus::List;
            app.clear_search(backend);
        }
        return;
    }

    ctx.input(|i| {
        if i.key_pressed(Key::J) || i.key_pressed(Key::ArrowDown) {
            app.move_selection(1, backend);
        }
        if i.key_pressed(Key::K) || i.key_pressed(Key::ArrowUp) {
            app.move_selection(-1, backend);
        }
        if i.key_pressed(Key::Slash) {
            app.focus = Focus::Search;
        }
        if i.key_pressed(Key::Escape) {
            app.clear_search(backend);
        }
        if i.key_pressed(Key::U) {
            app.toggle_read(backend);
        }
        if i.key_pressed(Key::F) {
            app.toggle_flagged(backend);
        }
        if i.key_pressed(Key::S) {
            app.move_to("archive", backend);
        }
        if i.key_pressed(Key::D) {
            app.move_to("trash", backend);
        }
        if i.key_pressed(Key::Z) {
            app.snooze("tomorrow", backend);
        }
        // `@` in the terminal; here it is a plain letter, since a GUI has a
        // button for this and the key is a convenience.
        if i.key_pressed(Key::R) {
            app.sync(backend);
        }
    });
}

/// `[2] https://…` from the body's link list, split into its two halves.
///
/// Only a line that is exactly a number and a web URL: a message quoting one
/// of ours, or writing something that merely looks like one, should render as
/// the text it is.
fn footnote(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (number, rest) = rest.split_once("] ")?;
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let url = rest.trim();
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return None;
    }
    // A URL with a space in it is two things, and one of them is not a URL.
    if url.contains(char::is_whitespace) {
        return None;
    }
    Some((&line[..number.len() + 2], url))
}

fn sender_of(message: &MessageSummary) -> String {
    message
        .from
        .first()
        .map(|a| match &a.name {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => a.address.clone(),
        })
        .unwrap_or_else(|| "(unknown sender)".to_string())
}

fn addresses(list: &[mailbox_api::Address]) -> String {
    list.iter()
        .map(|a| match &a.name {
            Some(name) if !name.trim().is_empty() => name.clone(),
            _ => a.address.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `2026-09-01T12:00:00Z` → `Sep 01`. Enough to place a message in a list.
fn short_date(rfc3339: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = rfc3339.split(['-', 'T']);
    let (_, month, day) = (parts.next(), parts.next(), parts.next());
    match (month.and_then(|m| m.parse::<usize>().ok()), day) {
        (Some(m), Some(d)) if (1..=12).contains(&m) => format!("{} {d}", MONTHS[m - 1]),
        _ => rfc3339.to_string(),
    }
}

//! Renders the message view to an off-screen terminal and prints it with
//! colour, so a change to the body renderer can be looked at.
//!
//! `cargo run -p mailbox-tui --example preview -- <message-id>`

use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let id = std::env::args().nth(1).expect("a message id");
    let opts = mailbox_core::session::SessionOptions {
        read_only: true,
        ..Default::default()
    };
    let workspace = mailbox_core::Workspace::open(
        &opts,
        mailbox_core::Tracer::disabled(),
        mailbox_core::Clock::from_env(),
    )?;

    let detail = workspace.show(
        &mailbox_core::session::IdRef::Id(id),
        &mailbox_core::render::RenderOptions {
            format: mailbox_api::BodyFormat::Md,
            ..Default::default()
        },
        false,
    )?;

    let mut app = mailbox_tui::App::new();
    app.view = mailbox_tui::View::Message;
    app.open = detail.message;

    let mut terminal = Terminal::new(TestBackend::new(96, 34))?;
    let keymap = mailbox_tui::Keymap::vim();
    terminal.draw(|f| mailbox_tui::ui::draw(f, &mut app, &keymap))?;

    print!("{}", ansi(terminal.backend().buffer()));
    Ok(())
}

/// Turns the off-screen buffer into escape codes a terminal will paint.
fn ansi(buffer: &ratatui::buffer::Buffer) -> String {
    use ratatui::style::{Color, Modifier};

    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let mut codes: Vec<String> = Vec::new();
            if let Some(c) = colour(cell.fg) {
                codes.push(c.to_string());
            }
            if cell.modifier.contains(Modifier::BOLD) {
                codes.push("1".into());
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                codes.push("3".into());
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                codes.push("4".into());
            }
            if cell.modifier.contains(Modifier::REVERSED) {
                codes.push("7".into());
            }
            out.push_str(&format!("\x1b[0;{}m{}", codes.join(";"), cell.symbol()));
        }
        out.push_str("\x1b[0m\n");
    }
    let _: Option<Color> = None;
    out
}

fn colour(c: ratatui::style::Color) -> Option<u8> {
    use ratatui::style::Color::*;
    Some(match c {
        Black => 30,
        Red => 31,
        Green => 32,
        Yellow => 33,
        Blue => 34,
        Magenta => 35,
        Cyan => 36,
        Gray => 37,
        DarkGray => 90,
        LightRed => 91,
        LightGreen => 92,
        LightYellow => 93,
        LightBlue => 94,
        LightMagenta => 95,
        LightCyan => 96,
        White => 97,
        _ => return None,
    })
}

//! The interface's colours, in one place.
//!
//! Named by role rather than by colour, and drawn from the terminal's own
//! sixteen: a palette that ignores the user's theme looks wrong in every
//! terminal but the one it was picked in.

use ratatui::style::Color;

pub const UNREAD: Color = Color::Blue;
pub const FLAGGED: Color = Color::Yellow;
pub const SENDER: Color = Color::Cyan;
pub const DIM: Color = Color::DarkGray;
/// The header's background — a band, not a colour of its own, so it reads as
/// part of the frame rather than as something that happened.
pub const HEADER_BG: Color = Color::Indexed(236);
pub const ACCENT: Color = Color::Green;
pub const WARN: Color = Color::Yellow;

/// Inline code and preformatted blocks.
pub const CODE: Color = Color::Magenta;
/// A heading in a rendered body.
pub const HEADING: Color = Color::Cyan;
/// A link's text.
pub const LINK: Color = Color::Blue;

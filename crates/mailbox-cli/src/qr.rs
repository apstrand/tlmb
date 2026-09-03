//! A QR code as terminal text.
//!
//! Used by pairing (§8, transport 1) and by `serve` (§3), which prints one so a
//! phone on the same LAN can open the interface without anyone typing a token
//! by hand.

/// Renders a link as a QR block.
///
/// Two terminal rows per QR row, using half-block characters, so the code stays
/// square and fits a normal window — a QR drawn one row per row is twice as
/// tall as it is wide and most phones will not read it.
pub fn render(text: &str) -> String {
    use qrcode::{EcLevel, QrCode};

    let Ok(code) = QrCode::with_error_correction_level(text, EcLevel::L) else {
        return String::new();
    };
    let width = code.width();
    let modules = code.to_colors();
    let dark = |x: usize, y: usize| -> bool {
        y < width && x < width && modules[y * width + x] == qrcode::Color::Dark
    };

    // A quiet zone is required by the specification; without it many readers
    // simply fail.
    const QUIET: usize = 2;
    let span = width + QUIET * 2;
    let mut out = String::new();

    for row in (0..span).step_by(2) {
        for col in 0..span {
            let x = col.wrapping_sub(QUIET);
            let upper = row >= QUIET && dark(x, row - QUIET);
            let lower = row + 1 >= QUIET && dark(x, row + 1 - QUIET);
            out.push(match (upper, lower) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
    }
    out
}

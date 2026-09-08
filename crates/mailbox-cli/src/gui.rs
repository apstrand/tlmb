//! The desktop shell (§7): a native window around the shared DOM client.
//!
//! Thin, as §4 requires. The window, the local server it talks to, and the
//! rule that a link in mail opens in the platform browser all live in
//! `mailbox-ui`. What is here is the one line that hands the session over.

use mailbox_core::error::Result;

use crate::commands::Ctx;

/// Runs the interface until the window closes.
pub fn run(ctx: &Ctx<'_>) -> Result<()> {
    mailbox_ui::run(ctx.opts.clone(), ctx.tracer.clone(), ctx.clock.clone())
}

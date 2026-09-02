//! **The terminal interface** (§7).
//!
//! > mutt-like muscle memory: `j/k`, `/` search, `t` tag, `d` delete, `s` save,
//! > `m` compose, `r`/`R` reply, `|` pipe to a command. Vim-ish by default,
//! > with an Emacs keymap and a fully remappable `keys.toml`. Everything is
//! > incremental: the index list paints before sync completes; searching
//! > filters as you type; no modal "please wait" states, ever.
//!
//! §4 makes frontends thin: everything this crate does is a call into
//! `mailbox-core`. The split inside it is between [`app`], which is the state
//! machine and knows nothing about terminals, and [`ui`], which draws. That is
//! what lets §7's behaviour be tested by pressing actions rather than bytes.

pub mod app;
pub mod keys;
pub mod run;
pub mod ui;

pub use app::{App, Effect, View};
pub use keys::{Action, Keymap};
pub use run::run;

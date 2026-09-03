//! **The graphical interface** (§7), shared by the desktop shell and the
//! browser.
//!
//! > **Web** — the GUI compiled to wasm, served locally, same keybindings,
//! > works offline via the local store on the serving host.
//!
//! One implementation, two shells (§3). The split inside it is the same one
//! the TUI makes: [`app`] is the state machine and knows nothing about
//! drawing, [`view`] draws it, and [`backend`] is the line across which the
//! desktop's store and the browser's `fetch` look identical.
//!
//! Nothing here depends on `mailbox-core`. That is not tidiness — the store is
//! SQLite and does not exist in a browser, so the interface has to be built
//! from the contract types alone or there could not be one interface.

pub mod app;
pub mod backend;
pub mod view;

pub use app::{App, Focus};
pub use backend::{Backend, Request, Response, Seq};

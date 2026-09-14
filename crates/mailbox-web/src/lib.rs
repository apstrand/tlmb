//! **The local web interface** (§3).
//!
//! > The web interface is compiled into every build. `mailbox serve` starts it;
//! > by default it binds `127.0.0.1` on an ephemeral port, prints a URL with a
//! > one-time token, and prints a QR code for that URL so a phone on the same
//! > LAN can open it.
//!
//! What this crate is: an HTTP server that answers with the §6.4 contract — the
//! same JSON `--json` prints — and serves the shared UI. `mailbox serve` hands
//! that UI to a browser; `mailbox gui` loads it in a Tauri webview against a
//! loopback copy of the same server. What it is not: a way in. It listens on
//! loopback, it demands a token, and it holds no session beyond the process
//! (§9).

pub mod api;
pub mod assets;
pub mod http;
pub mod server;
pub mod site;

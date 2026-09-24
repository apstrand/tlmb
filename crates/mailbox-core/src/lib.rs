//! `mailbox-core` — the store, the sync engine, the query language, threading,
//! and MIME. No I/O policy, no UI (§4).
//!
//! The rule that keeps this crate honest: if a frontend needs logic that isn't
//! here, that logic belongs here. The CLI in `mailbox-cli` is argument parsing
//! and rendering; the TUI, GUI, and web shells will be the same.

pub mod accounts;
pub mod attach;
pub mod bench;
pub mod build;
pub mod clipboard;
pub mod clock;
pub mod compose;
pub mod config;
pub mod credentials;
pub mod discover;
pub mod dns;
pub mod error;
pub mod fixture;
pub mod flags;
pub mod hex;
pub mod history;
pub mod https;
pub mod id;
pub mod imap;
pub mod mime;
pub mod notify;
pub mod oauth;
pub mod open;
pub mod owner;
pub mod pair;
pub mod paths;
pub mod policy;
pub mod providers;
pub mod query;
pub mod render;
pub mod send;
pub mod session;
pub mod smtp;
pub mod state;
pub mod storage;
pub mod store;
pub mod threading;
pub mod tls;
pub mod trace;
#[cfg(target_arch = "wasm32")]
pub mod wasm_stream;
pub mod workspace;

pub use clock::{Clock, Rng, Timestamp};
pub use error::{Error, Result};
pub use session::Session;
pub use store::Store;
pub use trace::Tracer;
pub use workspace::Workspace;

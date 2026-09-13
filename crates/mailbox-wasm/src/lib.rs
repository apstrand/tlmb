//! Stage 0 of the hosted flavour: prove that the existing core can cross the
//! browser boundary before committing to persistence or a complete transport.

use std::io::Cursor;

use mailbox_core::query;
use mailbox_core::store::read::ListOptions;
use mailbox_core::{Clock, Store, Tracer};
use wasm_bindgen::prelude::*;

/// The existing JSON facade with a wasm-bindgen ABI in place of the C ABI.
///
/// Its path-backed constructor is enough to prove the facade compiles. It is
/// not used by the spike page because browser persistence belongs to Stage 2.
#[wasm_bindgen(js_name = Mailbox)]
pub struct WasmMailbox {
    inner: mailbox_ffi::Mailbox,
}

#[wasm_bindgen(js_class = Mailbox)]
impl WasmMailbox {
    #[wasm_bindgen(constructor)]
    pub fn open(config_path: &str, state_dir: &str) -> Result<WasmMailbox, JsError> {
        let inner = mailbox_ffi::Mailbox::open(config_path, state_dir)
            .map_err(|e| JsError::new(&e.to_string()))?;
        Ok(WasmMailbox { inner })
    }

    pub fn exec(&self, method: &str, args_json: &str) -> Result<String, JsError> {
        self.inner.exec(method, args_json).map_err(|e| {
            JsError::new(
                &serde_json::to_string(&e).unwrap_or_else(|_| "mailbox request failed".to_string()),
            )
        })
    }
}

/// Opens the production schema in SQLite-wasm and executes an FTS5-backed L0
/// query. An empty result is expected; successful preparation and execution is
/// what proves the schema (including its virtual table) works in the browser.
#[wasm_bindgen]
pub fn l0_probe() -> Result<String, JsError> {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    let store = Store::open_in_memory("wasm-spike", Tracer::disabled(), Clock::fixed(0))
        .map_err(js_error)?;
    let node = query::parse("hosted", &Clock::fixed(0)).map_err(js_error)?;
    let listing = store
        .search(&node, &ListOptions::default())
        .map_err(js_error)?;

    let raw = b"Subject: Hosted probe\r\n\r\nBrowser core";
    let message = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| JsError::new("mail-parser rejected the probe message"))?;
    let imap_ok = imap_proto::parser::parse_response(b"* OK ready\r\n").is_ok();
    let compressed = zstd::encode_all(&raw[..], 1).map_err(js_error)?;
    let zstd_ok = zstd::decode_all(&compressed[..]).map_err(js_error)? == raw;
    let mut derived = [0_u8; 16];
    argon2::Argon2::default()
        .hash_password_into(b"hosted", b"mailbox-stage-0", &mut derived)
        .map_err(js_error)?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(js_error)?;
    let epoch_year = time::OffsetDateTime::from_unix_timestamp(0)
        .map_err(js_error)?
        .year();

    serde_json::to_string(&serde_json::json!({
        "ok": true,
        "schema": mailbox_core::store::schema::SCHEMA_VERSION,
        "fts5_query": "hosted",
        "matches": listing.messages.len(),
        "mail_parser": message.subject(),
        "imap_proto": imap_ok,
        "zstd": zstd_ok,
        "blake3": blake3::hash(raw).to_hex().to_string(),
        "argon2": derived.iter().any(|byte| *byte != 0),
        "time": epoch_year,
        "getrandom": random.iter().any(|byte| *byte != 0),
    }))
    .map_err(js_error)
}

/// A TLS state machine whose input and output are opaque TLS records. The JS
/// page sends these bytes as binary WebSocket frames; the relay therefore sees
/// neither certificates' plaintext nor IMAP credentials.
#[wasm_bindgen]
pub struct TlsProbe {
    connection: rustls::ClientConnection,
}

#[wasm_bindgen]
impl TlsProbe {
    #[wasm_bindgen(constructor)]
    pub fn new(host: &str) -> Result<TlsProbe, JsError> {
        let connection = mailbox_core::tls::client_connection(host).map_err(js_error)?;
        Ok(TlsProbe { connection })
    }

    #[wasm_bindgen(js_name = takeOutgoing)]
    pub fn take_outgoing(&mut self) -> Result<Vec<u8>, JsError> {
        let mut bytes = Vec::new();
        while self.connection.wants_write() {
            self.connection.write_tls(&mut bytes).map_err(js_error)?;
        }
        Ok(bytes)
    }

    /// Feeds one relay frame into rustls and reports whether the handshake is
    /// complete. Call `takeOutgoing` after every feed, including the last one.
    pub fn receive(&mut self, ciphertext: &[u8]) -> Result<bool, JsError> {
        let mut input = Cursor::new(ciphertext);
        while input.position() < ciphertext.len() as u64 {
            let read = self.connection.read_tls(&mut input).map_err(js_error)?;
            if read == 0 {
                break;
            }
        }
        self.connection.process_new_packets().map_err(js_error)?;
        Ok(!self.connection.is_handshaking())
    }
}

fn js_error(error: impl std::fmt::Display) -> JsError {
    JsError::new(&error.to_string())
}

/// Runs the real blocking IMAP connection path in a dedicated Worker. The
/// page owns the WebSocket and services `BrowserStream` through these shared
/// arrays while this call is sleeping in `Atomics.wait`.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn worker_imap_probe(
    control: js_sys::Int32Array,
    data: js_sys::Uint8Array,
    host: &str,
    port: u16,
) -> Result<String, JsError> {
    mailbox_core::wasm_stream::install(control, data).map_err(js_error)?;
    let connection = mailbox_core::imap::Connection::connect(
        &mailbox_core::imap::ConnectionConfig {
            host: host.to_string(),
            port,
            security: mailbox_core::imap::Security::Implicit,
            require_tls: true,
            timeout: std::time::Duration::from_secs(10),
        },
        Tracer::disabled(),
    )
    .map_err(js_error)?;

    serde_json::to_string(&serde_json::json!({
        "ok": true,
        "host": host,
        "port": port,
        "capabilities": connection.capabilities,
    }))
    .map_err(js_error)
}

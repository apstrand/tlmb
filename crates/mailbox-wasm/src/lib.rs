//! Stage 0 of the hosted flavour: prove that the existing core can cross the
//! browser boundary before committing to persistence or a complete transport.

use std::io::Cursor;
use std::sync::Arc;

use mailbox_core::query;
use mailbox_core::storage::MemoryStorage;
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

/// A production-schema SQLite database whose durable representation is an
/// opaque byte image. JavaScript persists that image in OPFS after mutations.
#[wasm_bindgen]
pub struct BrowserStore {
    store: Store,
    storage: Arc<MemoryStorage>,
}

#[wasm_bindgen]
impl BrowserStore {
    #[wasm_bindgen(constructor)]
    pub fn open(checkpoint: Option<Vec<u8>>) -> Result<BrowserStore, JsError> {
        let (database, files) = decode_checkpoint(checkpoint.as_deref()).map_err(js_error)?;
        let storage = Arc::new(MemoryStorage::decode(files.as_deref()).map_err(js_error)?);
        let store = Store::open_serialized_with_storage(
            database.as_deref(),
            storage.clone(),
            "browser",
            Tracer::disabled(),
            Clock::fixed(0),
        )
        .map_err(js_error)?;
        Ok(BrowserStore { store, storage })
    }

    /// Idempotent mutation used by the Stage 2 page to prove a reload restored
    /// state rather than merely opening another fresh schema.
    #[wasm_bindgen(js_name = ensureProbeFolder)]
    pub fn ensure_probe_folder(&mut self) -> Result<u32, JsError> {
        self.store
            .upsert_folder(&mailbox_core::store::FolderRecord {
                path: "Hosted persistence probe".into(),
                name: "Hosted persistence probe".into(),
                selectable: true,
                ..Default::default()
            })
            .map_err(js_error)?;
        self.store.folder_count().map_err(js_error)
    }

    #[wasm_bindgen(js_name = folderCount)]
    pub fn folder_count(&self) -> Result<u32, JsError> {
        self.store.folder_count().map_err(js_error)
    }

    /// Exercises the non-SQLite half of the storage seam with a compressed
    /// blob and `state.toml`. The `restored` bit is true only after reopening
    /// a checkpoint that contained both.
    #[wasm_bindgen(js_name = storageProbe)]
    pub fn storage_probe(&self) -> Result<String, JsError> {
        const RAW: &[u8] = b"Subject: Hosted storage probe\r\n\r\nOPFS bytes";
        let blob_id = mailbox_core::id::blob_id(RAW);
        let state = self.store.sync_state();
        let restored = self.store.blobs().has(&blob_id) && state.account == "browser";
        let stored_id = self.store.blobs().put(RAW).map_err(js_error)?;
        let mut state = state;
        state.account = "browser".into();
        self.store.save_sync_state(&state).map_err(js_error)?;
        let blob_ok = self.store.blobs().get(&stored_id).map_err(js_error)? == RAW;
        serde_json::to_string(&serde_json::json!({
            "ok": blob_ok,
            "restored": restored,
            "blob": stored_id,
        }))
        .map_err(js_error)
    }

    /// One atomic OPFS payload containing SQLite plus all byte-store entries.
    pub fn snapshot(&self) -> Result<Vec<u8>, JsError> {
        let database = self.store.serialize().map_err(js_error)?;
        let files = self.storage.encode().map_err(js_error)?;
        let mut checkpoint = b"MBCHECK1".to_vec();
        checkpoint.extend_from_slice(&(database.len() as u64).to_le_bytes());
        checkpoint.extend_from_slice(&database);
        checkpoint.extend_from_slice(&files);
        Ok(checkpoint)
    }
}

fn decode_checkpoint(image: Option<&[u8]>) -> std::io::Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    let Some(image) = image.filter(|bytes| !bytes.is_empty()) else {
        return Ok((None, None));
    };
    // Accept the Stage 2 prototype's raw SQLite image so existing previews
    // migrate forward without clearing browser storage.
    if image.starts_with(b"SQLite format 3\0") {
        return Ok((Some(image.to_vec()), None));
    }
    if image.len() < 16 || &image[..8] != b"MBCHECK1" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid browser checkpoint",
        ));
    }
    let database_len = usize::try_from(u64::from_le_bytes(image[8..16].try_into().unwrap()))
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "browser database image is too large",
            )
        })?;
    let files_start = 16usize.checked_add(database_len).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "browser checkpoint length overflow",
        )
    })?;
    if image.len() < files_start {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "truncated browser checkpoint",
        ));
    }
    Ok((
        Some(image[16..files_start].to_vec()),
        Some(image[files_start..].to_vec()),
    ))
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

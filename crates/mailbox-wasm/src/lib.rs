//! Stage 0 of the hosted flavour: prove that the existing core can cross the
//! browser boundary before committing to persistence or a complete transport.

use std::collections::BTreeMap;
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

/// A decoded checkpoint: the SQLite image and the encoded byte store.
type Checkpoint = (Option<Vec<u8>>, Option<Vec<u8>>);

fn decode_checkpoint(image: Option<&[u8]>) -> std::io::Result<Checkpoint> {
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

/// The production JSON facade backed by browser-restored configuration and
/// store bytes. Calls are identical to the mobile FFI surface.
#[wasm_bindgen]
pub struct HostedMailbox {
    inner: mailbox_ffi::Mailbox,
    storage: BTreeMap<String, Arc<MemoryStorage>>,
}

#[wasm_bindgen]
impl HostedMailbox {
    #[wasm_bindgen(constructor)]
    pub fn open(
        config_json: &str,
        credential: Option<String>,
        checkpoint: Option<Vec<u8>>,
    ) -> Result<HostedMailbox, JsError> {
        #[cfg(target_arch = "wasm32")]
        console_error_panic_hook::set_once();
        let config: mailbox_core::config::Config =
            serde_json::from_str(config_json).map_err(js_error)?;
        if config.accounts.len() > 1 {
            return Err(JsError::new(
                "Use openAccounts to restore multiple accounts.",
            ));
        }
        let mut credentials = BTreeMap::new();
        let mut checkpoints = BTreeMap::new();
        if let Some(id) = config.accounts.keys().next() {
            if let Some(password) = credential {
                credentials.insert(id.clone(), password);
            }
            if let Some(image) = checkpoint {
                checkpoints.insert(id.clone(), image);
            }
        }
        Self::restore_accounts(config, credentials, checkpoints).map_err(js_error)
    }

    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen(js_name = openAccounts)]
    pub fn open_accounts(
        config_json: &str,
        credentials_json: &str,
        checkpoints: js_sys::Object,
    ) -> Result<HostedMailbox, JsError> {
        console_error_panic_hook::set_once();
        let config = serde_json::from_str(config_json).map_err(js_error)?;
        let credentials = serde_json::from_str(credentials_json).map_err(js_error)?;
        let mut images = BTreeMap::new();
        for key in js_sys::Object::keys(&checkpoints).iter() {
            let id = key
                .as_string()
                .ok_or_else(|| JsError::new("invalid checkpoint account"))?;
            let value = js_sys::Reflect::get(&checkpoints, &key)
                .map_err(|_| JsError::new("cannot read account checkpoint"))?;
            let bytes = value
                .dyn_into::<js_sys::Uint8Array>()
                .map_err(|_| JsError::new("checkpoint must be a Uint8Array"))?;
            images.insert(id, bytes.to_vec());
        }
        Self::restore_accounts(config, credentials, images).map_err(js_error)
    }

    #[cfg(target_arch = "wasm32")]
    pub fn snapshots(&self) -> Result<js_sys::Object, JsError> {
        let images = self.checkpoint_images().map_err(js_error)?;
        let result = js_sys::Object::create(JsValue::NULL.unchecked_ref());
        for (id, bytes) in images {
            js_sys::Reflect::set(
                &result,
                &JsValue::from_str(&id),
                &js_sys::Uint8Array::from(bytes.as_slice()),
            )
            .map_err(|_| JsError::new("cannot write account checkpoint"))?;
        }
        Ok(result)
    }

    pub fn exec(&self, method: &str, args_json: &str) -> Result<String, JsError> {
        self.inner.exec(method, args_json).map_err(|error| {
            JsError::new(
                &serde_json::to_string(&error)
                    .unwrap_or_else(|_| "mailbox request failed".to_string()),
            )
        })
    }

    pub fn snapshot(&self) -> Result<Vec<u8>, JsError> {
        if self.storage.len() > 1 {
            return Err(JsError::new("Use snapshots for multiple accounts."));
        }
        Ok(self
            .checkpoint_images()
            .map_err(js_error)?
            .into_values()
            .next()
            .unwrap_or_default())
    }
}

impl HostedMailbox {
    fn restore_accounts(
        config: mailbox_core::config::Config,
        credentials: BTreeMap<String, String>,
        checkpoints: BTreeMap<String, Vec<u8>>,
    ) -> mailbox_core::error::Result<Self> {
        let mut sessions = Vec::new();
        let mut stores = BTreeMap::new();
        for account_id in config.accounts.keys() {
            if let Some(password) = credentials
                .get(account_id)
                .filter(|value| !value.is_empty())
            {
                mailbox_core::credentials::install(
                    account_id,
                    mailbox_core::imap::Credential::Password(password.clone()),
                );
            }
            let (database, files) =
                decode_checkpoint(checkpoints.get(account_id).map(Vec::as_slice))?;
            let storage = Arc::new(MemoryStorage::decode(files.as_deref())?);
            let tracer = Tracer::disabled();
            let clock = Clock::from_env();
            let store = Store::open_serialized_with_storage(
                database.as_deref(),
                storage.clone(),
                account_id,
                tracer.clone(),
                clock.clone(),
            )?;
            let session = mailbox_core::session::Session::from_store(
                config.clone(),
                account_id.clone(),
                store,
                tracer,
                clock,
            )?;
            sessions.push(session);
            stores.insert(account_id.clone(), storage);
        }
        Ok(HostedMailbox {
            inner: mailbox_ffi::Mailbox::from_workspace(mailbox_core::Workspace::from_sessions(
                config, sessions,
            )),
            storage: stores,
        })
    }

    fn checkpoint_images(&self) -> mailbox_core::error::Result<BTreeMap<String, Vec<u8>>> {
        self.inner
            .database_snapshots()?
            .into_iter()
            .map(|(id, database)| {
                let storage = self.storage.get(&id).expect("every session has storage");
                let mut image = b"MBCHECK1".to_vec();
                image.extend_from_slice(&(database.len() as u64).to_le_bytes());
                image.extend_from_slice(&database);
                image.extend_from_slice(&storage.encode()?);
                Ok((id, image))
            })
            .collect()
    }
}

#[cfg(test)]
mod hosted_tests {
    use super::*;
    use mailbox_core::config::{AccountConfig, Config};
    use mailbox_core::fixture::Fixture;

    #[test]
    fn multiple_account_checkpoints_restore_unified_mail_and_route_mutations() {
        let mut config = Config::default();
        let mut images = BTreeMap::new();
        let mut message_ids = BTreeMap::new();
        for id in ["home", "work"] {
            config.accounts.insert(
                id.to_string(),
                AccountConfig {
                    address: format!("{id}@example.com"),
                    imap_host: Some(format!("imap.{id}.example")),
                    ..Default::default()
                },
            );
            let mut mailbox =
                HostedMailbox::restore_accounts(config.clone(), BTreeMap::new(), images.clone())
                    .unwrap();
            // Seed only the newly added account; the first remains restored.
            let storage = mailbox.storage.get(id).unwrap().clone();
            let mut store = Store::open_serialized_with_storage(
                None,
                storage.clone(),
                id,
                Tracer::disabled(),
                Clock::fixed(0),
            )
            .unwrap();
            let fixture = Fixture::parse(&format!(
                r#"
[[folder]]
path = "INBOX"
special_use = "inbox"
[[message]]
name = "mail"
folder = "INBOX"
from = "sender@example.com"
to = ["{id}@example.com"]
subject = "Mail for {id}"
date = "2026-08-01T10:00:00Z"
message_id = "<mail@example.com>"
body = "Body for {id}"
"#
            ))
            .unwrap();
            message_ids.insert(
                id.to_string(),
                fixture.seed_store(&mut store).unwrap()["mail"].clone(),
            );
            let session = mailbox_core::session::Session::from_store(
                config.clone(),
                id,
                store,
                Tracer::disabled(),
                Clock::fixed(0),
            )
            .unwrap();
            mailbox.inner = mailbox_ffi::Mailbox::from_workspace(
                mailbox_core::Workspace::from_sessions(config.clone(), vec![session]),
            );
            images.extend(mailbox.checkpoint_images().unwrap());
        }
        assert_ne!(message_ids["home"], message_ids["work"]);
        let mailbox =
            HostedMailbox::restore_accounts(config.clone(), BTreeMap::new(), images).unwrap();
        let list: serde_json::Value =
            serde_json::from_str(&mailbox.inner.exec("ls", "{}").unwrap()).unwrap();
        assert_eq!(list["messages"].as_array().unwrap().len(), 2);
        mailbox
            .inner
            .exec(
                "flag",
                &serde_json::json!({
                    "ids": [message_ids["work"]], "changes": ["+seen"]
                })
                .to_string(),
            )
            .unwrap();
        let images = mailbox.checkpoint_images().unwrap();
        assert_eq!(images.len(), 2);
        let restored = HostedMailbox::restore_accounts(config, BTreeMap::new(), images).unwrap();
        let list: serde_json::Value =
            serde_json::from_str(&restored.inner.exec("ls", "{}").unwrap()).unwrap();
        for message in list["messages"].as_array().unwrap() {
            let seen = message["flags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|flag| flag == "seen");
            assert_eq!(seen, message["account"] == "work");
            let shown: serde_json::Value = serde_json::from_str(
                &restored
                    .inner
                    .exec(
                        "show",
                        &serde_json::json!({
                            "id": message["id"], "format": "text"
                        })
                        .to_string(),
                    )
                    .unwrap(),
            )
            .unwrap();
            assert!(shown.to_string().contains(&format!(
                "Body for {}",
                message["account"].as_str().unwrap()
            )));
        }
    }
}

#[wasm_bindgen(js_name = prepareAccount)]
pub fn prepare_account(args_json: &str) -> Result<String, JsError> {
    let input: HostedAccountInput = serde_json::from_str(args_json).map_err(js_error)?;
    let password = input.password.clone();
    let (id, account, discovered) =
        mailbox_core::accounts::prepare(&input.into()).map_err(js_error)?;
    let mut config = mailbox_core::config::Config::default();
    config.accounts.insert(id.clone(), account);
    serde_json::to_string(&serde_json::json!({
        "id": id,
        "config": config,
        "discovered": discovered,
        "has_credential": password.is_some_and(|value| !value.is_empty()),
    }))
    .map_err(js_error)
}

#[wasm_bindgen(js_name = testAccount)]
pub fn test_account(args_json: &str) -> Result<String, JsError> {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();
    let input: HostedAccountInput = serde_json::from_str(args_json).map_err(js_error)?;
    let password = input
        .password
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| JsError::new("an account test needs a password"))?;
    let (id, account, _) = mailbox_core::accounts::prepare(&input.into()).map_err(js_error)?;
    let verified = mailbox_core::accounts::verify(
        &id,
        &account,
        &mailbox_core::imap::Credential::Password(password),
        Tracer::disabled(),
        std::time::Duration::from_secs(30),
    )
    .map_err(js_error)?;
    serde_json::to_string(&verified).map_err(js_error)
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = installBrowserStream)]
pub fn install_browser_stream(
    control: js_sys::Int32Array,
    data: js_sys::Uint8Array,
) -> Result<(), JsError> {
    mailbox_core::wasm_stream::install(control, data).map_err(js_error)
}

#[derive(Clone, serde::Deserialize)]
struct HostedAccountInput {
    #[serde(default)]
    id: String,
    address: String,
    imap_host: Option<String>,
    imap_port: Option<u16>,
    imap_security: Option<String>,
    imap_user: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    smtp_security: Option<String>,
    smtp_user: Option<String>,
    password: Option<String>,
    require_tls: Option<bool>,
}

impl From<HostedAccountInput> for mailbox_core::accounts::NewAccount {
    fn from(input: HostedAccountInput) -> Self {
        mailbox_core::accounts::NewAccount {
            id: input.id,
            address: input.address,
            imap_host: input.imap_host,
            imap_port: input.imap_port,
            imap_security: input.imap_security,
            imap_user: input.imap_user,
            smtp_host: input.smtp_host,
            smtp_port: input.smtp_port,
            smtp_security: input.smtp_security,
            smtp_user: input.smtp_user,
            password: input.password,
            require_tls: input.require_tls,
            ..Default::default()
        }
    }
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

use std::path::Path;
use std::sync::Arc;

use mailbox_core::config::{AccountConfig, Config};
use mailbox_core::history::History;
use mailbox_core::oauth::{self, Tokens};
use mailbox_core::policy::Policy;
use mailbox_core::storage::{ByteStorage, MemoryStorage};
use mailbox_core::store::{Blobs, FolderState, SyncState};
use mailbox_core::{Clock, Tracer};

#[test]
fn browser_storage_image_round_trips_every_core_file_kind() {
    let storage = Arc::new(MemoryStorage::new());

    let blobs = Blobs::with_storage(storage.clone(), "blobs");
    let raw = b"Subject: stored in OPFS\r\n\r\nhello";
    let blob_id = blobs.put(raw).unwrap();

    let mut state = SyncState {
        account: "personal".into(),
        ..Default::default()
    };
    state.set_folder(
        "INBOX",
        FolderState {
            uidvalidity: 42,
            max_uid: 9,
            ..Default::default()
        },
    );
    state
        .save_to(storage.as_ref(), Path::new("state.toml"))
        .unwrap();

    let mut config = Config::default();
    config.accounts.insert(
        "personal".into(),
        AccountConfig {
            address: "me@example.com".into(),
            ..Default::default()
        },
    );
    config
        .save_to(storage.as_ref(), Path::new("config.toml"))
        .unwrap();
    storage
        .write(Path::new("policy.toml"), b"[send]\nmax_recipients = 3\n")
        .unwrap();
    History::save_to(
        storage.as_ref(),
        Path::new("listing.json"),
        &["message-id".into()],
    );
    oauth::cache_in(
        storage.as_ref(),
        Path::new("oauth.json"),
        &Tokens {
            access_token: "short-lived".into(),
            refresh_token: None,
            expires_at: 123,
        },
    )
    .unwrap();

    let tracer = Tracer::to_storage(
        storage.clone(),
        "trace.ndjson",
        Clock::fixed(0),
        false,
        None,
    );
    tracer.emit("storage.test", serde_json::json!({"ok": true}));

    let image = storage.encode().unwrap();
    let restored = Arc::new(MemoryStorage::decode(Some(&image)).unwrap());
    let restored_blobs = Blobs::with_storage(restored.clone(), "blobs");
    assert_eq!(restored_blobs.get(&blob_id).unwrap(), raw);
    assert_eq!(
        SyncState::load_from(restored.as_ref(), Path::new("state.toml"))
            .folder("INBOX")
            .max_uid,
        9
    );
    assert_eq!(
        Config::load_from(restored.as_ref(), Path::new("config.toml"))
            .unwrap()
            .account("personal")
            .unwrap()
            .address,
        "me@example.com"
    );
    assert_eq!(
        Policy::load_from(restored.as_ref(), Path::new("policy.toml"))
            .unwrap()
            .send
            .max_recipients,
        Some(3)
    );
    assert_eq!(
        History::load_from(restored.as_ref(), Path::new("listing.json")).listing,
        ["message-id"]
    );
    assert_eq!(
        oauth::cached_in(restored.as_ref(), Path::new("oauth.json"))
            .unwrap()
            .access_token,
        "short-lived"
    );
    let trace = restored.read(Path::new("trace.ndjson")).unwrap().unwrap();
    assert!(String::from_utf8(trace).unwrap().contains("storage.test"));
}

#[test]
fn browser_storage_rejects_paths_that_escape_its_root() {
    let storage = MemoryStorage::new();
    assert!(storage.write(Path::new("../secret"), b"no").is_err());
    assert!(storage.write(Path::new("/absolute"), b"no").is_err());
}

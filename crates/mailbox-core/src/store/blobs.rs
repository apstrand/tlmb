//! The content-addressed blob store: `blobs/ab/cdef…`, zstd-compressed raw MIME
//! (§5.1).
//!
//! Content addressing is what deduplicates a message that appears in `INBOX`
//! and `[Gmail]/All Mail` down to one copy on disk. It also makes writes
//! idempotent, which is why a crash between the blob write and the `meta.db`
//! commit is recoverable by simply replaying the delta (§14.3's crash-only
//! oracle).

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::Result;
use crate::id;
use crate::storage::{ByteStorage, FsStorage};

/// Compression level. 3 is zstd's default and the knee of the curve for mail:
/// higher levels buy a few percent for several times the CPU, on a workload
/// that compresses every message the sync engine touches.
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone)]
pub struct Blobs {
    storage: Arc<dyn ByteStorage>,
    prefix: PathBuf,
}

impl Blobs {
    pub fn new(root: impl Into<PathBuf>) -> Blobs {
        Blobs {
            storage: Arc::new(FsStorage::new(root.into())),
            prefix: PathBuf::new(),
        }
    }

    pub fn with_storage(storage: Arc<dyn ByteStorage>, prefix: impl Into<PathBuf>) -> Blobs {
        Blobs {
            storage,
            prefix: prefix.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.prefix
    }

    fn path_for(&self, blob_id: &str) -> PathBuf {
        self.prefix.join(id::blob_path(blob_id))
    }

    /// Stores raw MIME bytes and returns their content address.
    ///
    /// Writing an id that already exists is a no-op rather than a rewrite: the
    /// bytes are the same by construction, and skipping the write keeps a
    /// re-download of a large mailbox from rewriting the whole store.
    pub fn put(&self, raw: &[u8]) -> Result<String> {
        let blob_id = id::blob_id(raw);
        let path = self.path_for(&blob_id);
        if self.storage.exists(&path) {
            return Ok(blob_id);
        }

        let compressed = zstd::encode_all(raw, ZSTD_LEVEL)
            .map_err(|e| crate::Error::store(format!("compressing blob: {e}")))?;
        // Atomic in both implementations: native uses write-then-rename and
        // browser storage replaces one map entry before its OPFS checkpoint.
        self.storage.write_atomic(&path, &compressed)?;
        Ok(blob_id)
    }

    pub fn get(&self, blob_id: &str) -> Result<Vec<u8>> {
        let path = self.path_for(blob_id);
        let compressed = self
            .storage
            .read(&path)?
            .ok_or_else(|| crate::Error::not_found("blob", blob_id))?;
        let mut out = Vec::new();
        zstd::Decoder::new(Cursor::new(compressed))
            .map_err(|e| crate::Error::store(format!("opening blob {blob_id}: {e}")))?
            .read_to_end(&mut out)
            .map_err(|e| crate::Error::store(format!("reading blob {blob_id}: {e}")))?;
        Ok(out)
    }

    pub fn has(&self, blob_id: &str) -> bool {
        self.storage.exists(&self.path_for(blob_id))
    }

    /// Removes a blob. Callers must have established that no `messages` row
    /// still references it — content addressing means several rows can.
    pub fn remove(&self, blob_id: &str) -> Result<()> {
        self.storage.remove(&self.path_for(blob_id))?;
        Ok(())
    }

    /// Bytes on disk, for `mailbox status`.
    pub fn disk_usage(&self) -> u64 {
        self.storage.usage(&self.prefix)
    }
}

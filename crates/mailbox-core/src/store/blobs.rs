//! The content-addressed blob store: `blobs/ab/cdef…`, zstd-compressed raw MIME
//! (§5.1).
//!
//! Content addressing is what deduplicates a message that appears in `INBOX`
//! and `[Gmail]/All Mail` down to one copy on disk. It also makes writes
//! idempotent, which is why a crash between the blob write and the `meta.db`
//! commit is recoverable by simply replaying the delta (§14.3's crash-only
//! oracle).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::id;

/// Compression level. 3 is zstd's default and the knee of the curve for mail:
/// higher levels buy a few percent for several times the CPU, on a workload
/// that compresses every message the sync engine touches.
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone)]
pub struct Blobs {
    root: PathBuf,
}

impl Blobs {
    pub fn new(root: impl Into<PathBuf>) -> Blobs {
        Blobs { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, blob_id: &str) -> PathBuf {
        self.root.join(id::blob_path(blob_id))
    }

    /// Stores raw MIME bytes and returns their content address.
    ///
    /// Writing an id that already exists is a no-op rather than a rewrite: the
    /// bytes are the same by construction, and skipping the write keeps a
    /// re-download of a large mailbox from rewriting the whole store.
    pub fn put(&self, raw: &[u8]) -> Result<String> {
        let blob_id = id::blob_id(raw);
        let path = self.path_for(&blob_id);
        if path.exists() {
            return Ok(blob_id);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let compressed = zstd::encode_all(raw, ZSTD_LEVEL)
            .map_err(|e| crate::Error::store(format!("compressing blob: {e}")))?;

        // Write-then-rename, so a reader never sees a partial blob and a crash
        // leaves at worst a temp file. The temp name carries the blob id, so
        // two processes writing the same blob do not collide on the temp path
        // in a way that could interleave.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, &compressed)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => {}
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // A concurrent writer winning the race is success, not failure:
                // content addressing means their bytes are ours.
                if !path.exists() {
                    return Err(e.into());
                }
            }
        }
        Ok(blob_id)
    }

    pub fn get(&self, blob_id: &str) -> Result<Vec<u8>> {
        let path = self.path_for(blob_id);
        let file = fs::File::open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => crate::Error::not_found("blob", blob_id),
            _ => crate::Error::Io(e),
        })?;
        let mut out = Vec::new();
        zstd::Decoder::new(file)
            .map_err(|e| crate::Error::store(format!("opening blob {blob_id}: {e}")))?
            .read_to_end(&mut out)
            .map_err(|e| crate::Error::store(format!("reading blob {blob_id}: {e}")))?;
        Ok(out)
    }

    pub fn has(&self, blob_id: &str) -> bool {
        self.path_for(blob_id).exists()
    }

    /// Removes a blob. Callers must have established that no `messages` row
    /// still references it — content addressing means several rows can.
    pub fn remove(&self, blob_id: &str) -> Result<()> {
        match fs::remove_file(self.path_for(blob_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Bytes on disk, for `mailbox status`.
    pub fn disk_usage(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            let Ok(entries) = fs::read_dir(dir) else {
                return 0;
            };
            entries
                .flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => walk(&e.path()),
                    Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
                    Err(_) => 0,
                })
                .sum()
        }
        walk(&self.root)
    }
}

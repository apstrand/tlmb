//! The byte-storage seam shared by native and hosted mailbox flavours.
//!
//! Paths passed to a [`ByteStorage`] are logical, relative names. Native
//! builds map them below a directory; the browser keeps them in memory while
//! JavaScript checkpoints the encoded map to OPFS. SQLite has its own
//! serialization API, but blobs and small sidecar files use this interface.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// Where non-SQLite mailbox bytes live.
pub trait ByteStorage: std::fmt::Debug + Send + Sync {
    fn read(&self, key: &Path) -> io::Result<Option<Vec<u8>>>;
    fn write(&self, key: &Path, bytes: &[u8]) -> io::Result<()>;
    fn write_atomic(&self, key: &Path, bytes: &[u8]) -> io::Result<()>;
    fn remove(&self, key: &Path) -> io::Result<()>;
    fn exists(&self, key: &Path) -> bool;
    fn usage(&self, prefix: &Path) -> u64;
}

fn valid_key(key: &Path) -> io::Result<()> {
    if key.as_os_str().is_empty()
        || key.is_absolute()
        || key
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid storage key `{}`", key.display()),
        ));
    }
    Ok(())
}

/// Native storage rooted at a directory, retaining today's file layout.
#[derive(Debug, Clone)]
pub struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    pub fn new(root: impl Into<PathBuf>) -> FsStorage {
        FsStorage { root: root.into() }
    }

    fn path(&self, key: &Path) -> io::Result<PathBuf> {
        valid_key(key)?;
        Ok(self.root.join(key))
    }

    fn prepare(path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn restrict(path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        #[cfg(not(unix))]
        let _ = path;
        Ok(())
    }
}

impl ByteStorage for FsStorage {
    fn read(&self, key: &Path) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write(&self, key: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        Self::prepare(&path)?;
        std::fs::write(&path, bytes)?;
        Self::restrict(&path)
    }

    fn write_atomic(&self, key: &Path, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(key)?;
        Self::prepare(&path)?;
        let mut tmp = path.clone();
        let extension = path
            .extension()
            .map(|value| format!("{}.mailbox-tmp", value.to_string_lossy()))
            .unwrap_or_else(|| "mailbox-tmp".to_string());
        tmp.set_extension(extension);
        std::fs::write(&tmp, bytes)?;
        Self::restrict(&tmp)?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(&tmp);
                Err(error)
            }
        }
    }

    fn remove(&self, key: &Path) -> io::Result<()> {
        match std::fs::remove_file(self.path(key)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn exists(&self, key: &Path) -> bool {
        self.path(key).is_ok_and(|path| path.exists())
    }

    fn usage(&self, prefix: &Path) -> u64 {
        fn walk(path: &Path) -> u64 {
            if path.is_file() {
                return path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            }
            let Ok(entries) = std::fs::read_dir(path) else {
                return 0;
            };
            entries.flatten().map(|entry| walk(&entry.path())).sum()
        }

        if prefix.as_os_str().is_empty() {
            walk(&self.root)
        } else {
            self.path(prefix).map_or(0, |path| walk(&path))
        }
    }
}

/// Browser storage while the worker is running.
///
/// [`MemoryStorage::encode`] is the durable boundary: the generated client
/// atomically saves that image to OPFS alongside SQLite's serialized image.
#[derive(Debug, Default)]
pub struct MemoryStorage {
    entries: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
}

impl MemoryStorage {
    pub fn new() -> MemoryStorage {
        MemoryStorage::default()
    }

    pub fn decode(image: Option<&[u8]>) -> io::Result<MemoryStorage> {
        let mut entries = BTreeMap::new();
        let Some(mut input) = image.filter(|bytes| !bytes.is_empty()) else {
            return Ok(MemoryStorage::new());
        };
        if input.len() < 8 || &input[..8] != b"MBSTORE1" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid browser storage image",
            ));
        }
        input = &input[8..];
        while !input.is_empty() {
            let key_len = take_u32(&mut input)? as usize;
            let value_len = usize::try_from(take_u64(&mut input)?).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "storage entry is too large")
            })?;
            let entry_len = key_len.checked_add(value_len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "storage entry length overflow")
            })?;
            if input.len() < entry_len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated browser storage image",
                ));
            }
            let key = std::str::from_utf8(&input[..key_len])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let path = PathBuf::from(key);
            valid_key(&path)?;
            input = &input[key_len..];
            entries.insert(path, input[..value_len].to_vec());
            input = &input[value_len..];
        }
        Ok(MemoryStorage {
            entries: Mutex::new(entries),
        })
    }

    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| io::Error::other("browser storage lock poisoned"))?;
        let mut out = b"MBSTORE1".to_vec();
        for (key, value) in entries.iter() {
            let key = key.to_string_lossy();
            let key_len = u32::try_from(key.len())
                .map_err(|_| io::Error::other("browser storage key is too long"))?;
            out.extend_from_slice(&key_len.to_le_bytes());
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(value);
        }
        Ok(out)
    }
}

impl ByteStorage for MemoryStorage {
    fn read(&self, key: &Path) -> io::Result<Option<Vec<u8>>> {
        valid_key(key)?;
        Ok(self
            .entries
            .lock()
            .map_err(|_| io::Error::other("browser storage lock poisoned"))?
            .get(key)
            .cloned())
    }

    fn write(&self, key: &Path, bytes: &[u8]) -> io::Result<()> {
        valid_key(key)?;
        self.entries
            .lock()
            .map_err(|_| io::Error::other("browser storage lock poisoned"))?
            .insert(key.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn write_atomic(&self, key: &Path, bytes: &[u8]) -> io::Result<()> {
        self.write(key, bytes)
    }

    fn remove(&self, key: &Path) -> io::Result<()> {
        valid_key(key)?;
        self.entries
            .lock()
            .map_err(|_| io::Error::other("browser storage lock poisoned"))?
            .remove(key);
        Ok(())
    }

    fn exists(&self, key: &Path) -> bool {
        valid_key(key).is_ok()
            && self
                .entries
                .lock()
                .is_ok_and(|entries| entries.contains_key(key))
    }

    fn usage(&self, prefix: &Path) -> u64 {
        self.entries.lock().map_or(0, |entries| {
            entries
                .iter()
                .filter(|(key, _)| prefix.as_os_str().is_empty() || key.starts_with(prefix))
                .map(|(_, value)| value.len() as u64)
                .sum()
        })
    }
}

fn take_u32(input: &mut &[u8]) -> io::Result<u32> {
    if input.len() < 4 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "missing u32"));
    }
    let (number, rest) = input.split_at(4);
    *input = rest;
    Ok(u32::from_le_bytes(number.try_into().unwrap()))
}

fn take_u64(input: &mut &[u8]) -> io::Result<u64> {
    if input.len() < 8 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "missing u64"));
    }
    let (number, rest) = input.split_at(8);
    *input = rest;
    Ok(u64::from_le_bytes(number.try_into().unwrap()))
}

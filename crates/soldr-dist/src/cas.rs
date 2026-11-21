//! Persistent verified content store. Access timestamps survive worker restarts.
use crate::{InputFile, Result};
use soldr_platform::fs::{identity, permissions};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone)]
pub struct Cas {
    root: PathBuf,
    budget: u64,
    pins: std::sync::Arc<soldr_platform::sync::Mutex<HashMap<String, usize>>>,
}
impl Cas {
    pub fn new(root: &Path, budget: u64) -> Result<Self> {
        let root = root.join("cas");
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            budget,
            pins: std::sync::Arc::new(soldr_platform::sync::Mutex::new(HashMap::new())),
        })
    }
    pub fn lease(&self, digests: impl IntoIterator<Item = String>) -> CasLease {
        let mut lease = CasLease {
            cas: self.clone(),
            digests: Vec::new(),
        };
        for digest in digests {
            lease.pin(digest);
        }
        lease
    }
    pub fn path(&self, digest: &str) -> Result<PathBuf> {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("invalid blake3 digest".into());
        }
        Ok(self.root.join(&digest[..2]).join(digest))
    }
    pub fn contains(&self, digest: &str) -> Result<bool> {
        Ok(self.path(digest)?.is_file())
    }
    pub fn open(&self, digest: &str) -> Result<fs::File> {
        let file = fs::File::open(self.path(digest)?)?;
        filetime::set_file_atime(self.path(digest)?, filetime::FileTime::now())?;
        Ok(file)
    }
    pub fn get(&self, digest: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open(digest)?.read_to_end(&mut bytes)?;
        if blake3::hash(&bytes).to_hex().as_str() != digest {
            return Err("CAS digest mismatch".into());
        }
        Ok(bytes)
    }
    pub fn verify(&self, digest: &str) -> Result<()> {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(self.open(digest)?)?;
        if hasher.finalize().to_hex().as_str() != digest {
            return Err("CAS digest mismatch".into());
        }
        Ok(())
    }
    pub fn ingest(&self, path: &Path) -> Result<(String, u64)> {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(fs::File::open(path)?)?;
        let digest = hasher.finalize().to_hex().to_string();
        let file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        self.receive(&digest, size, file)?;
        Ok((digest, size))
    }
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let digest = blake3::hash(bytes).to_hex().to_string();
        self.receive(&digest, bytes.len() as u64, bytes)?;
        Ok(digest)
    }
    pub fn receive(&self, digest: &str, size: u64, mut reader: impl Read) -> Result<()> {
        let path = self.path(digest)?;
        fs::create_dir_all(path.parent().ok_or("CAS parent missing")?)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        let mut hasher = blake3::Hasher::new();
        let mut received = 0u64;
        let mut buf = [0; 65536];
        loop {
            let count = reader.read(&mut buf)?;
            if count == 0 {
                break;
            }
            received = received
                .checked_add(count as u64)
                .ok_or("blob size overflow")?;
            if received > size {
                return Err("blob exceeds declared size".into());
            }
            hasher.update(&buf[..count]);
            temp.write_all(&buf[..count])?;
        }
        if received != size || hasher.finalize().to_hex().as_str() != digest {
            return Err("blob size/digest mismatch".into());
        }
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    }
    pub async fn receive_stream(
        &self,
        digest: &str,
        size: u64,
        reader: &mut iroh::endpoint::RecvStream,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let path = self.path(digest)?;
        fs::create_dir_all(path.parent().ok_or("CAS parent missing")?)?;
        let temp = tempfile::NamedTempFile::new_in(&self.root)?;
        let mut file = tokio::fs::File::from_std(temp.reopen()?);
        let mut hasher = blake3::Hasher::new();
        let mut remaining = size;
        let mut buf = [0; 65536];
        while remaining > 0 {
            let limit = remaining.min(buf.len() as u64) as usize;
            let count = reader
                .read(&mut buf[..limit])
                .await?
                .ok_or("truncated blob")?;
            hasher.update(&buf[..count]);
            file.write_all(&buf[..count]).await?;
            remaining -= count as u64;
        }
        if reader.read(&mut buf[..1]).await?.is_some() {
            return Err("blob exceeds declared size".into());
        }
        if hasher.finalize().to_hex().as_str() != digest {
            return Err("blob digest mismatch".into());
        }
        file.sync_all().await?;
        drop(file);
        temp.persist(path)?;
        Ok(())
    }
    pub fn entries(&self) -> Result<Vec<(PathBuf, u64, SystemTime)>> {
        let mut rows = Vec::new();
        for dir in fs::read_dir(&self.root)? {
            let dir = dir?;
            if !dir.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(dir.path())? {
                let entry = entry?;
                let meta = match entry.metadata() {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                };
                if meta.is_file() {
                    rows.push((entry.path(), meta.len(), meta.accessed()?));
                }
            }
        }
        Ok(rows)
    }
    pub fn size(&self) -> Result<u64> {
        Ok(self.entries()?.iter().map(|(_, size, _)| size).sum())
    }
    /// Active job digests are pinned; evict unrelated entries even under sustained load.
    pub fn evict(&self) -> Result<()> {
        let mut rows = self.entries()?;
        let mut total: u64 = rows.iter().map(|(_, size, _)| size).sum();
        rows.sort_by_key(|(_, _, time)| *time);
        for (path, size, _) in rows {
            if total <= self.budget {
                break;
            }
            let pins = self.pins.lock();
            let digest = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or("invalid CAS entry")?;
            if pins.contains_key(digest) {
                continue;
            }
            match fs::remove_file(path) {
                Ok(()) => total -= size,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => total -= size,
                Err(e) => return Err(e.into()),
            }
            drop(pins);
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct DigestMemo {
    entries: HashMap<PathBuf, MemoEntry>,
}
struct MemoEntry {
    modified: SystemTime,
    size: u64,
    identity: Option<identity::FileIdentity>,
    digest: String,
}
impl DigestMemo {
    pub fn input(&mut self, path: &Path) -> Result<InputFile> {
        let manifest_path = crate::sandbox::absolute(&std::env::current_dir()?, path)?;
        let path = fs::canonicalize(path)?;
        let before = fs::metadata(&path)?;
        let modified = before.modified()?;
        let id = identity::file_identity(&path);
        let cached = self
            .entries
            .get(&path)
            .filter(|e| e.modified == modified && e.size == before.len() && e.identity == id);
        let digest = if let Some(entry) = cached {
            entry.digest.clone()
        } else {
            let mut hasher = blake3::Hasher::new();
            hasher.update_reader(fs::File::open(&path)?)?;
            let after = fs::metadata(&path)?;
            if before.len() != after.len()
                || modified != after.modified()?
                || id != identity::file_identity(&path)
            {
                return Err("input changed while hashing".into());
            }
            let digest = hasher.finalize().to_hex().to_string();
            self.entries.insert(
                path.clone(),
                MemoEntry {
                    modified,
                    size: before.len(),
                    identity: id,
                    digest: digest.clone(),
                },
            );
            digest
        };
        Ok(InputFile {
            abs_path: manifest_path.to_str().ok_or("non-UTF8 input path")?.into(),
            blake3: digest,
            size: before.len(),
            executable: permissions::mode(&path).is_some_and(|m| m & 0o111 != 0),
        })
    }
}

/// A job's CAS references. Drop releases pins on success, error, cancellation or disconnect.
pub struct CasLease {
    cas: Cas,
    digests: Vec<String>,
}
impl CasLease {
    pub fn pin(&mut self, digest: String) {
        *self.cas.pins.lock().entry(digest.clone()).or_default() += 1;
        self.digests.push(digest);
    }
    pub fn ingest(&mut self, path: &Path) -> Result<(String, u64)> {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(fs::File::open(path)?)?;
        let digest = hasher.finalize().to_hex().to_string();
        self.pin(digest.clone());
        let file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        self.cas.receive(&digest, size, file)?;
        Ok((digest, size))
    }
}
impl Drop for CasLease {
    fn drop(&mut self) {
        let mut pins = self.cas.pins.lock();
        for digest in &self.digests {
            if let Some(count) = pins.get_mut(digest) {
                *count -= 1;
                if *count == 0 {
                    pins.remove(digest);
                }
            }
        }
    }
}

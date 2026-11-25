use crate::error::Result;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use walkdir::WalkDir;

pub const TMP_SUFFIX: &str = ".sessionshare.tmp";

/// Computes SHA-256 hex digest of bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Atomically writes `bytes` to `target` with Windows file-lock retry/backoff
/// (handles brief locks from OneDrive, Google Drive, or Windows Defender).
pub fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_name = format!(
        "{}.{}{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file"),
        uuid::Uuid::new_v4().simple(),
        TMP_SUFFIX
    );
    let tmp_path = target.with_file_name(tmp_name);

    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }

    let mut last_err = None;
    for attempt in 0..5 {
        match fs::rename(&tmp_path, target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // On Windows, fs::rename may fail if target exists and is momentarily locked;
                // try removing target and retrying rename.
                if target.exists() {
                    let _ = fs::remove_file(target);
                    if fs::rename(&tmp_path, target).is_ok() {
                        return Ok(());
                    }
                }
                last_err = Some(e);
                thread::sleep(Duration::from_millis(60 * (attempt + 1)));
            }
        }
    }

    let _ = fs::remove_file(&tmp_path);
    Err(last_err
        .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "atomic write failed"))
        .into())
}

/// Reads a file with short retry/backoff for Windows synced-drive locks.
pub fn read_with_retry(path: &Path) -> Result<Vec<u8>> {
    let mut last_err = None;
    for attempt in 0..5 {
        match fs::read(path) {
            Ok(data) => return Ok(data),
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(50 * (attempt + 1)));
            }
        }
    }
    Err(last_err.unwrap().into())
}

/// Pluggable storage backend trait. Currently implemented for a local synced drive folder.
pub trait StorageProvider: Send + Sync {
    fn exists(&self, rel_path: &str) -> bool;
    fn read_bytes(&self, rel_path: &str) -> Result<Option<Vec<u8>>>;
    fn write_bytes(&self, rel_path: &str, bytes: &[u8]) -> Result<()>;
    fn list_files(&self, rel_prefix: &str) -> Result<Vec<String>>;
    fn modified_time(&self, rel_path: &str) -> Option<std::time::SystemTime>;
}

/// Stores normalized bundles inside an already-approved local synced drive folder
/// (e.g., `C:\Users\<you>\OneDrive\SessionShare`).
#[derive(Debug, Clone)]
pub struct LocalFolderProvider {
    root: PathBuf,
}

impl LocalFolderProvider {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, rel_path: &str) -> PathBuf {
        let mut p = self.root.clone();
        for part in rel_path.split(['/', '\\']).filter(|s| !s.is_empty() && *s != "..") {
            p.push(part);
        }
        p
    }
}

impl StorageProvider for LocalFolderProvider {
    fn exists(&self, rel_path: &str) -> bool {
        self.resolve(rel_path).is_file()
    }

    fn read_bytes(&self, rel_path: &str) -> Result<Option<Vec<u8>>> {
        let p = self.resolve(rel_path);
        if !p.is_file() {
            return Ok(None);
        }
        Ok(Some(read_with_retry(&p)?))
    }

    fn write_bytes(&self, rel_path: &str, bytes: &[u8]) -> Result<()> {
        let p = self.resolve(rel_path);
        atomic_write(&p, bytes)
    }

    fn list_files(&self, rel_prefix: &str) -> Result<Vec<String>> {
        let base = self.resolve(rel_prefix);
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in WalkDir::new(&base).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name.ends_with(TMP_SUFFIX) || name.starts_with('.') {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                let posix = rel.to_string_lossy().replace('\\', "/");
                out.push(posix);
            }
        }
        out.sort();
        Ok(out)
    }

    fn modified_time(&self, rel_path: &str) -> Option<std::time::SystemTime> {
        fs::metadata(self.resolve(rel_path)).ok()?.modified().ok()
    }
}

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

    // `fs::rename` replaces an existing target atomically on Windows (MoveFileEx + REPLACE_EXISTING).
    // Never delete the target first: if the follow-up rename failed, both copies would be lost.
    let mut last_err = None;
    for attempt in 0..RENAME_ATTEMPTS {
        match fs::rename(&tmp_path, target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)));
            }
        }
    }

    // Only the temp file is removed; the original target is left untouched.
    let _ = fs::remove_file(&tmp_path);
    Err(last_err
        .unwrap_or_else(|| std::io::Error::other("atomic write failed"))
        .into())
}

const RENAME_ATTEMPTS: usize = 6;

/// Cheap change-detection fingerprint (`<size>:<mtime-nanos>`) for a local file.
pub fn local_fingerprint(path: &Path) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}:{}", meta.len(), mtime))
}

/// Returns true if `path` was modified within the last `secs` seconds.
pub fn modified_within(path: &Path, secs: u64) -> bool {
    if secs == 0 {
        return false;
    }
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|age| age < Duration::from_secs(secs))
        .unwrap_or(false)
}

/// Moves `src` to `dest` (creating parents), falling back to copy + delete across volumes.
pub fn move_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    let bytes = read_with_retry(src)?;
    atomic_write(dest, &bytes)?;
    fs::remove_file(src)?;
    Ok(())
}

/// Format used for timestamped batch folders inside trash / conflict backup roots.
pub const BATCH_FORMAT: &str = "%Y%m%d-%H%M%S";

/// Deletes timestamped batch folders (see [`BATCH_FORMAT`]) under `root` older than `days`.
pub fn purge_old_batches(root: &Path, days: u64) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let cutoff = chrono::Utc::now().naive_utc() - chrono::Duration::days(days as i64);
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(ts) = chrono::NaiveDateTime::parse_from_str(&name, BATCH_FORMAT) else {
            continue;
        };
        if ts < cutoff && fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
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
    /// Appends `bytes` to an existing stored file in place, so drives that sync differentially
    /// only transfer the new tail instead of the whole file.
    fn append_bytes(&self, rel_path: &str, bytes: &[u8]) -> Result<()> {
        let mut all = self.read_bytes(rel_path)?.unwrap_or_default();
        all.extend_from_slice(bytes);
        self.write_bytes(rel_path, &all)
    }
    fn list_files(&self, rel_prefix: &str) -> Result<Vec<String>>;
    fn modified_time(&self, rel_path: &str) -> Option<std::time::SystemTime>;
    /// Change-detection fingerprint (size + mtime) of a stored file, if it exists.
    fn fingerprint(&self, rel_path: &str) -> Option<String>;
    /// Moves a stored file into the provider's trash under the given batch folder.
    fn trash(&self, rel_path: &str, batch: &str) -> Result<()>;
}

pub const REMOTE_TRASH_DIR: &str = ".trash";

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

    fn append_bytes(&self, rel_path: &str, bytes: &[u8]) -> Result<()> {
        let p = self.resolve(rel_path);
        let mut last_err = None;
        for attempt in 0..RENAME_ATTEMPTS {
            match fs::OpenOptions::new().append(true).open(&p) {
                Ok(mut f) => {
                    f.write_all(bytes)?;
                    f.sync_all()?;
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return atomic_write(&p, bytes);
                }
                Err(e) => {
                    last_err = Some(e);
                    thread::sleep(Duration::from_millis(50 * (attempt as u64 + 1)));
                }
            }
        }
        Err(last_err
            .unwrap_or_else(|| std::io::Error::other("append failed"))
            .into())
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

    fn fingerprint(&self, rel_path: &str) -> Option<String> {
        local_fingerprint(&self.resolve(rel_path))
    }

    fn trash(&self, rel_path: &str, batch: &str) -> Result<()> {
        let src = self.resolve(rel_path);
        if !src.is_file() {
            return Ok(());
        }
        let dest = self.resolve(&format!("{}/{}/{}", REMOTE_TRASH_DIR, batch, rel_path));
        move_file(&src, &dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(windows)]
    #[test]
    fn atomic_write_keeps_original_when_target_is_locked() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("data.jsonl");
        fs::write(&target, b"original").unwrap();

        // Exclusive handle (no sharing) makes every rename attempt fail.
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        assert!(atomic_write(&target, b"new").is_err());
        drop(lock);

        assert_eq!(fs::read(&target).unwrap(), b"original");
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(TMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn trash_moves_file_into_batch_folder() {
        let tmp = TempDir::new().unwrap();
        let provider = LocalFolderProvider::new(tmp.path().to_path_buf()).unwrap();
        provider.write_bytes("global/skills/a.md", b"x").unwrap();
        provider.trash("global/skills/a.md", "20260101-000000").unwrap();
        assert!(!provider.exists("global/skills/a.md"));
        assert!(provider.exists(".trash/20260101-000000/global/skills/a.md"));
    }

    #[test]
    fn purges_only_expired_batches() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("20000101-000000")).unwrap();
        let fresh = chrono::Utc::now().format(BATCH_FORMAT).to_string();
        fs::create_dir_all(tmp.path().join(&fresh)).unwrap();
        fs::create_dir_all(tmp.path().join("not-a-batch")).unwrap();
        assert_eq!(purge_old_batches(tmp.path(), 30), 1);
        assert!(tmp.path().join(&fresh).is_dir());
        assert!(tmp.path().join("not-a-batch").is_dir());
    }
}

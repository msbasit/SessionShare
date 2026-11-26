use crate::error::Result;
use crate::models::{ActivityEntry, AppConfig, MergeConflict, PendingApproval};
use crate::storage::atomic_write;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};

/// Returns the default app data directory (`%APPDATA%\SessionShare`).
pub fn default_app_data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("SessionShare")
}

pub fn load_or_create_config(app_data_dir: &Path, default_claude: PathBuf) -> Result<AppConfig> {
    fs::create_dir_all(app_data_dir)?;
    let cfg_path = app_data_dir.join("config.json");
    if cfg_path.is_file() {
        let raw = fs::read_to_string(&cfg_path)?;
        let cfg: AppConfig = serde_json::from_str(&raw)?;
        return Ok(cfg);
    }
    let cfg = AppConfig::new_default(default_claude);
    save_config(app_data_dir, &cfg)?;
    Ok(cfg)
}

pub fn save_config(app_data_dir: &Path, cfg: &AppConfig) -> Result<()> {
    fs::create_dir_all(app_data_dir)?;
    let cfg_path = app_data_dir.join("config.json");
    let raw = serde_json::to_string_pretty(cfg)?;
    atomic_write(&cfg_path, raw.as_bytes())
}

/// Last-synced state of one synced file, keyed by its path inside the synced drive folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// Hash of the portable content both sides agreed on at the last sync.
    pub hash: String,
    /// Local file path this entry was synced against (deletions only propagate if it still matches).
    pub local_path: Option<String>,
    /// Local fingerprint after the last sync (`None` forces a full comparison next time).
    pub local_fp: Option<String>,
    /// Synced-drive fingerprint after the last sync.
    pub remote_fp: Option<String>,
}

pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    pub fn open(app_data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(app_data_dir)?;
        let db_path = app_data_dir.join("state.db");
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_sync_state (
                rel_key TEXT PRIMARY KEY,
                last_synced_hash TEXT NOT NULL,
                last_synced_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS activity_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                kind TEXT NOT NULL,
                path TEXT NOT NULL,
                detail TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_approvals (
                remote_key TEXT PRIMARY KEY,
                remote_hash TEXT NOT NULL,
                detected_at TEXT NOT NULL,
                summary TEXT NOT NULL,
                preview TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS approval_decisions (
                remote_key TEXT NOT NULL,
                remote_hash TEXT NOT NULL,
                approved INTEGER NOT NULL,
                PRIMARY KEY (remote_key, remote_hash)
            );
            CREATE TABLE IF NOT EXISTS file_base (
                rel_key TEXT PRIMARY KEY,
                hash TEXT NOT NULL,
                content BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS merge_conflicts (
                remote_key TEXT PRIMARY KEY,
                local_path TEXT NOT NULL,
                local_hash TEXT NOT NULL,
                remote_hash TEXT NOT NULL,
                has_base INTEGER NOT NULL,
                detected_at TEXT NOT NULL
            );",
        )?;
        let db = Self { conn };
        for col in ["local_path", "local_fp", "remote_fp"] {
            db.add_column_if_missing("file_sync_state", col)?;
        }
        Ok(db)
    }

    fn add_column_if_missing(&self, table: &str, column: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let exists = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .flatten()
            .any(|c| c == column);
        if !exists {
            self.conn.execute(
                &format!("ALTER TABLE {} ADD COLUMN {} TEXT", table, column),
                [],
            )?;
        }
        Ok(())
    }

    /// Binds the per-file sync state to a specific (synced folder, Claude folder) pair.
    /// If either changed, all file state is cleared so stale hashes never drive merges or deletions.
    /// Returns true when the state was reset.
    pub fn ensure_roots(&self, identity: &str) -> Result<bool> {
        let current: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'roots'", [], |r| r.get(0))
            .optional()?;
        if current.as_deref() == Some(identity) {
            return Ok(false);
        }
        // Without a recorded identity (fresh DB or upgrade from an older version) the existing
        // state can't be trusted either, so it is always cleared.
        self.conn.execute("DELETE FROM file_sync_state", [])?;
        self.conn.execute("DELETE FROM pending_approvals", [])?;
        self.conn.execute("DELETE FROM file_base", [])?;
        self.conn.execute("DELETE FROM merge_conflicts", [])?;
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('roots', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![identity],
        )?;
        Ok(current.is_some())
    }

    pub fn get_state(&self, rel_key: &str) -> Result<Option<FileState>> {
        let res = self
            .conn
            .query_row(
                "SELECT last_synced_hash, local_path, local_fp, remote_fp
                 FROM file_sync_state WHERE rel_key = ?1",
                params![rel_key],
                |row| {
                    Ok(FileState {
                        hash: row.get(0)?,
                        local_path: row.get(1)?,
                        local_fp: row.get(2)?,
                        remote_fp: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(res)
    }

    pub fn set_state(&self, rel_key: &str, state: &FileState) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO file_sync_state
                (rel_key, last_synced_hash, last_synced_at, local_path, local_fp, remote_fp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(rel_key) DO UPDATE SET
                last_synced_hash = excluded.last_synced_hash,
                last_synced_at = excluded.last_synced_at,
                local_path = excluded.local_path,
                local_fp = excluded.local_fp,
                remote_fp = excluded.remote_fp",
            params![
                rel_key,
                state.hash,
                now,
                state.local_path,
                state.local_fp,
                state.remote_fp
            ],
        )?;
        Ok(())
    }

    pub fn delete_state(&self, rel_key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM file_sync_state WHERE rel_key = ?1",
            params![rel_key],
        )?;
        Ok(())
    }

    pub fn record_activity(&self, entry: &ActivityEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO activity_log (timestamp, kind, path, detail) VALUES (?1, ?2, ?3, ?4)",
            params![
                entry.timestamp.to_rfc3339(),
                entry.kind,
                entry.path,
                entry.detail
            ],
        )?;
        // Keep last 500 entries
        self.conn.execute(
            "DELETE FROM activity_log WHERE id NOT IN (SELECT id FROM activity_log ORDER BY id DESC LIMIT 500)",
            [],
        )?;
        Ok(())
    }

    pub fn recent_activities(&self, limit: usize) -> Result<Vec<ActivityEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, kind, path, detail FROM activity_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let ts_str: String = row.get(0)?;
            Ok(ActivityEntry {
                timestamp: parse_ts(&ts_str),
                kind: row.get(1)?,
                path: row.get(2)?,
                detail: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Stores the content both PCs agreed on, used as the ancestor for later 3-way merges.
    pub fn set_base(&self, rel_key: &str, hash: &str, content: &[u8]) -> Result<()> {
        self.conn.execute(
            "INSERT INTO file_base (rel_key, hash, content) VALUES (?1, ?2, ?3)
             ON CONFLICT(rel_key) DO UPDATE SET hash = excluded.hash, content = excluded.content",
            params![rel_key, hash, content],
        )?;
        Ok(())
    }

    /// Returns the stored ancestor only if it is the version with the given hash.
    pub fn get_base(&self, rel_key: &str, hash: &str) -> Result<Option<Vec<u8>>> {
        let res = self
            .conn
            .query_row(
                "SELECT content FROM file_base WHERE rel_key = ?1 AND hash = ?2",
                params![rel_key, hash],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(res)
    }

    pub fn delete_base(&self, rel_key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM file_base WHERE rel_key = ?1", params![rel_key])?;
        Ok(())
    }

    /// Records a held conflict. Returns true if it is new or either side changed since it was recorded.
    pub fn upsert_conflict(&self, c: &MergeConflict) -> Result<bool> {
        let existing = self.get_conflict(&c.remote_key)?;
        if let Some(e) = &existing {
            if e.local_hash == c.local_hash && e.remote_hash == c.remote_hash {
                return Ok(false);
            }
        }
        self.conn.execute(
            "INSERT INTO merge_conflicts (remote_key, local_path, local_hash, remote_hash, has_base, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(remote_key) DO UPDATE SET
                local_path = excluded.local_path,
                local_hash = excluded.local_hash,
                remote_hash = excluded.remote_hash,
                has_base = excluded.has_base,
                detected_at = excluded.detected_at",
            params![
                c.remote_key,
                c.local_path,
                c.local_hash,
                c.remote_hash,
                c.has_base as i64,
                c.detected_at.to_rfc3339()
            ],
        )?;
        Ok(true)
    }

    pub fn clear_conflict(&self, rel_key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM merge_conflicts WHERE remote_key = ?1",
            params![rel_key],
        )?;
        Ok(())
    }

    fn conflict_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MergeConflict> {
        let ts: String = row.get(5)?;
        Ok(MergeConflict {
            remote_key: row.get(0)?,
            local_path: row.get(1)?,
            local_hash: row.get(2)?,
            remote_hash: row.get(3)?,
            has_base: row.get::<_, i64>(4)? != 0,
            detected_at: parse_ts(&ts),
        })
    }

    pub fn get_conflict(&self, rel_key: &str) -> Result<Option<MergeConflict>> {
        let res = self
            .conn
            .query_row(
                "SELECT remote_key, local_path, local_hash, remote_hash, has_base, detected_at
                 FROM merge_conflicts WHERE remote_key = ?1",
                params![rel_key],
                Self::conflict_from_row,
            )
            .optional()?;
        Ok(res)
    }

    pub fn conflicts(&self) -> Result<Vec<MergeConflict>> {
        let mut stmt = self.conn.prepare(
            "SELECT remote_key, local_path, local_hash, remote_hash, has_base, detected_at
             FROM merge_conflicts ORDER BY detected_at DESC",
        )?;
        let rows = stmt.query_map([], Self::conflict_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Returns `Some(true)` if approved, `Some(false)` if rejected, `None` if undecided.
    pub fn approval_decision(&self, rel_key: &str, hash: &str) -> Result<Option<bool>> {
        let res = self
            .conn
            .query_row(
                "SELECT approved FROM approval_decisions WHERE remote_key = ?1 AND remote_hash = ?2",
                params![rel_key, hash],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(res.map(|v| v != 0))
    }

    pub fn set_approval_decision(&self, rel_key: &str, hash: &str, approved: bool) -> Result<()> {
        // Older decisions for the same file are irrelevant once a newer version is decided.
        self.conn.execute(
            "DELETE FROM approval_decisions WHERE remote_key = ?1",
            params![rel_key],
        )?;
        self.conn.execute(
            "INSERT INTO approval_decisions (remote_key, remote_hash, approved) VALUES (?1, ?2, ?3)",
            params![rel_key, hash, approved as i64],
        )?;
        self.clear_pending(rel_key)
    }

    pub fn upsert_pending(&self, item: &PendingApproval) -> Result<()> {
        self.conn.execute(
            "INSERT INTO pending_approvals (remote_key, remote_hash, detected_at, summary, preview)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(remote_key) DO UPDATE SET
                detected_at = CASE WHEN pending_approvals.remote_hash = excluded.remote_hash
                                   THEN pending_approvals.detected_at ELSE excluded.detected_at END,
                remote_hash = excluded.remote_hash,
                summary = excluded.summary,
                preview = excluded.preview",
            params![
                item.remote_key,
                item.remote_hash,
                item.detected_at.to_rfc3339(),
                item.summary,
                item.preview
            ],
        )?;
        Ok(())
    }

    pub fn clear_pending(&self, rel_key: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM pending_approvals WHERE remote_key = ?1",
            params![rel_key],
        )?;
        Ok(())
    }

    pub fn pending_approvals(&self) -> Result<Vec<PendingApproval>> {
        let mut stmt = self.conn.prepare(
            "SELECT remote_key, remote_hash, detected_at, summary, preview
             FROM pending_approvals ORDER BY detected_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let ts: String = row.get(2)?;
            Ok(PendingApproval {
                remote_key: row.get(0)?,
                remote_hash: row.get(1)?,
                detected_at: parse_ts(&ts),
                summary: row.get(3)?,
                preview: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn changing_roots_clears_file_state() {
        let tmp = TempDir::new().unwrap();
        let db = StateDb::open(tmp.path()).unwrap();
        assert!(!db.ensure_roots("sync-a|claude").unwrap());
        let st = FileState {
            hash: "h".into(),
            local_path: Some("C:\\x".into()),
            local_fp: None,
            remote_fp: None,
        };
        db.set_state("global/CLAUDE.md", &st).unwrap();
        assert!(!db.ensure_roots("sync-a|claude").unwrap());
        assert_eq!(db.get_state("global/CLAUDE.md").unwrap(), Some(st));
        assert!(db.ensure_roots("sync-b|claude").unwrap());
        assert_eq!(db.get_state("global/CLAUDE.md").unwrap(), None);
    }
}

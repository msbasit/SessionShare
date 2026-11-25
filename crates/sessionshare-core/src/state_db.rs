use crate::error::Result;
use crate::models::{ActivityEntry, AppConfig};
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

#[derive(Debug, Clone)]
pub struct FileSyncRecord {
    pub rel_key: String,
    pub last_synced_hash: String,
    pub last_synced_at: String,
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
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn get_file_hash(&self, rel_key: &str) -> Result<Option<String>> {
        let res = self
            .conn
            .query_row(
                "SELECT last_synced_hash FROM file_sync_state WHERE rel_key = ?1",
                params![rel_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(res)
    }

    pub fn set_file_hash(&self, rel_key: &str, hash: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO file_sync_state (rel_key, last_synced_hash, last_synced_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(rel_key) DO UPDATE SET
                last_synced_hash = excluded.last_synced_hash,
                last_synced_at = excluded.last_synced_at",
            params![rel_key, hash, now],
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
            let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(ActivityEntry {
                timestamp,
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
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalSyncOptions {
    pub skills: bool,
    pub agents: bool,
    pub commands: bool,
    #[serde(default = "default_true")]
    pub plans: bool,
    #[serde(default = "default_true")]
    pub rules: bool,
    #[serde(default = "default_true")]
    pub hooks: bool,
    pub claude_md: bool,
    pub settings_json: bool,
    pub history_jsonl: bool,
    pub project_memory: bool,
}

fn default_true() -> bool {
    true
}

impl Default for GlobalSyncOptions {
    fn default() -> Self {
        Self {
            skills: true,
            agents: true,
            commands: true,
            plans: true,
            rules: true,
            hooks: true,
            claude_md: true,
            settings_json: true,
            history_jsonl: true,
            project_memory: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub machine_id: String,
    pub machine_name: String,
    pub claude_dir: PathBuf,
    pub sync_dir: Option<PathBuf>,
    pub auto_sync: bool,
    pub debounce_secs: u64,
    pub global_options: GlobalSyncOptions,
    /// Optional workspace parent folders (e.g. `C:\Dev`, `D:\Work`) to auto-discover
    /// matching local projects even if Claude Code hasn't been run in them yet.
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    /// Per-project opt-in/out map: canonical_project_id -> enabled
    pub project_sync_enabled: BTreeMap<String, bool>,
    /// Local path mapping: canonical_project_id -> local absolute path on this PC
    pub local_project_paths: BTreeMap<String, String>,
    /// How each local mapping was resolved ("git", "home-relative", "workspace-scan", "folder-name", "manual")
    #[serde(default)]
    pub mapping_sources: BTreeMap<String, String>,
    /// Fallback full sync interval in minutes when auto-sync is on (0 = disabled).
    #[serde(default = "default_periodic_sync_mins")]
    pub periodic_sync_mins: u64,
    /// Transcripts modified within this many seconds are treated as active and never rewritten locally.
    #[serde(default = "default_active_session_grace_secs")]
    pub active_session_grace_secs: u64,
    /// Propagate deletions between PCs (deleted files are moved to a trash folder, not erased).
    #[serde(default = "default_true")]
    pub propagate_deletions: bool,
    /// Incoming hook / permission / command changes require approval before being applied.
    #[serde(default = "default_true")]
    pub require_hook_approval: bool,
    /// Top-level `settings.json` keys that never leave this PC (e.g. `env` holding API keys).
    #[serde(default = "default_settings_excluded_keys")]
    pub settings_excluded_keys: Vec<String>,
    /// Days to keep trash and conflict backups before they are purged.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    /// Start SessionShare (minimized to tray) when signing in to Windows.
    #[serde(default)]
    pub launch_at_login: bool,
}

fn default_periodic_sync_mins() -> u64 {
    5
}

fn default_active_session_grace_secs() -> u64 {
    30
}

fn default_settings_excluded_keys() -> Vec<String> {
    vec!["env".to_string()]
}

fn default_retention_days() -> u64 {
    30
}

impl AppConfig {
    pub fn new_default(claude_dir: PathBuf) -> Self {
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Windows-PC".to_string());
        Self {
            machine_id: uuid::Uuid::new_v4().to_string(),
            machine_name: host,
            claude_dir,
            sync_dir: None,
            auto_sync: true,
            debounce_secs: 6,
            global_options: GlobalSyncOptions::default(),
            workspace_roots: Vec::new(),
            project_sync_enabled: BTreeMap::new(),
            local_project_paths: BTreeMap::new(),
            mapping_sources: BTreeMap::new(),
            periodic_sync_mins: default_periodic_sync_mins(),
            active_session_grace_secs: default_active_session_grace_secs(),
            propagate_deletions: true,
            require_hook_approval: true,
            settings_excluded_keys: default_settings_excluded_keys(),
            retention_days: default_retention_days(),
            launch_at_login: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Registry {
    pub version: u32,
    pub projects: BTreeMap<String, CanonicalProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalProject {
    pub id: String,
    pub display_name: String,
    pub git_remote_normalized: Option<String>,
    /// Portable home-relative hint (e.g. `Dev/MyApp` when located under `%USERPROFILE%\Dev\MyApp`)
    /// so another PC with a different Windows username can auto-resolve it.
    #[serde(default)]
    pub home_relative_hint: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineManifest {
    pub machine_id: String,
    pub machine_name: String,
    pub updated_at: DateTime<Utc>,
    /// canonical_project_id -> local path on that machine
    pub project_paths: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectView {
    pub canonical_id: String,
    pub display_name: String,
    pub git_remote: Option<String>,
    pub local_path: Option<String>,
    pub encoded_local_dir: Option<String>,
    pub mapping_source: Option<String>,
    pub remote_machines: Vec<String>,
    pub sync_enabled: bool,
    pub local_session_count: usize,
    pub remote_session_count: usize,
    pub needs_mapping: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub project_id: String,
    pub project_name: String,
    pub local_project_path: Option<String>,
    pub title: String,
    pub last_modified: Option<DateTime<Utc>>,
    pub line_count: usize,
    pub size_bytes: u64,
    pub in_local: bool,
    pub in_remote: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncProgress {
    pub phase: String,
    pub current_item: Option<String>,
    pub checked: usize,
    pub completed: usize,
    pub total: usize,
    pub upcoming: Vec<String>,
    pub recent: Vec<ActivityEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncReport {
    pub timestamp: DateTime<Utc>,
    pub pushed: usize,
    pub pulled: usize,
    pub merged: usize,
    pub auto_mapped: usize,
    /// Files moved to trash because they were deleted on the other side.
    #[serde(default)]
    pub deleted: usize,
    /// Local writes postponed because the file is in active use (e.g. a running Claude session).
    #[serde(default)]
    pub deferred: usize,
    /// Number of incoming changes waiting for user approval.
    #[serde(default)]
    pub pending_approvals: usize,
    /// Files changed differently on both PCs, held untouched until merged in the Review tab.
    #[serde(default)]
    pub unresolved_conflicts: usize,
    pub conflicts: Vec<String>,
    pub errors: Vec<String>,
    pub activities: Vec<ActivityEntry>,
}

/// An incoming change (hook script or settings.json hooks/permissions) held until the user approves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub remote_key: String,
    pub remote_hash: String,
    pub detected_at: DateTime<Utc>,
    pub summary: String,
    /// Incoming content (truncated) so the user can review it.
    pub preview: String,
}

/// A file changed differently on both PCs; both copies are left untouched until the user merges.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeConflict {
    pub remote_key: String,
    pub local_path: String,
    pub local_hash: String,
    pub remote_hash: String,
    /// Whether the version both PCs last agreed on is known (enables automatic pre-merging).
    pub has_base: bool,
    pub detected_at: DateTime<Utc>,
}

/// Everything the merge editor needs: both sides, the diff chunks, and hashes that guard
/// against saving a merge after either file changed again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictView {
    pub remote_key: String,
    pub local_path: String,
    /// `"json"` for settings.json (result must be valid JSON), otherwise `"text"`.
    pub kind: String,
    pub has_base: bool,
    pub local_hash: String,
    pub remote_hash: String,
    pub local_text: String,
    pub remote_text: String,
    pub chunks: Vec<crate::merge3::Chunk>,
}

use crate::claude_fs::{
    auto_resolve_unmapped_project, canonical_id_from_git, canonical_id_from_name,
    compute_home_relative_hint, default_home_dir, display_name_from_path, encode_project_path,
    inspect_session_file, normalize_windows_path, resolve_encoded_project_dir, scan_local_projects,
    SessionFileMeta,
};
use crate::error::{Result, SessionShareError};
use crate::merge3::{auto_result, merge2, merge3};
use crate::merger::{merge_jsonl, MergeOutcome};
use crate::models::{
    ActivityEntry, AppConfig, CanonicalProject, ConflictView, MachineManifest, MergeConflict,
    PendingApproval, ProjectView, Registry, SessionSummary, SyncProgress, SyncReport,
};
use crate::path_rewrite::{
    denormalize_history_jsonl, denormalize_jsonl_with_ctx, denormalize_settings_json,
    normalize_history_jsonl, normalize_jsonl_with_ctx, normalize_settings_json, PathRewriteContext,
};
use crate::settings::{command_signature, has_any_key, json_merge3, reinject_keys, strip_keys};
use crate::state_db::{save_config, FileState, StateDb};
use crate::storage::{
    atomic_write, local_fingerprint, modified_within, move_file, purge_old_batches,
    read_with_retry, sha256_hex, LocalFolderProvider, StorageProvider, BATCH_FORMAT,
    REMOTE_TRASH_DIR, TMP_SUFFIX,
};
use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

const REGISTRY_PATH: &str = "registry.json";
const SETTINGS_KEY: &str = "global/settings.json";
const HISTORY_KEY: &str = "global/history.jsonl";
const HOOKS_PREFIX: &str = "global/hooks/";
const PREVIEW_LIMIT: usize = 20_000;
const CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Deletion safety guard: when more than this many tracked files disappear from one side at once
/// and they make up at least half of the folder, deletions are not propagated.
const MASS_DELETE_MIN: usize = 5;
/// Largest file whose last-synced content is kept as a merge ancestor.
const BASE_LIMIT: usize = 1_000_000;

/// Inputs for resolving a file changed on both PCs.
struct GenericConflict<'a> {
    local_path: &'a Path,
    remote_key: &'a str,
    local: &'a [u8],
    remote: &'a [u8],
    local_hash: &'a str,
    remote_hash: &'a str,
    last_hash: Option<&'a str>,
    local_fp: &'a Option<String>,
    gated: bool,
}

/// Both versions of a held conflict in the form shown to the user.
struct ConflictSides {
    kind: &'static str,
    local_path: PathBuf,
    local_text: String,
    remote_text: String,
    base_text: Option<String>,
    local_hash: String,
    remote_hash: String,
    /// Raw local settings.json (needed to keep local-only keys when saving a merge).
    local_raw: Option<String>,
}

pub struct SyncEngine {
    app_data_dir: PathBuf,
    config: AppConfig,
    db: StateDb,
    /// Optional home dir override (primarily for deterministic cross-machine tests)
    home_dir_override: Option<String>,
    /// Session metadata cache keyed by file path, invalidated by fingerprint.
    session_meta_cache: HashMap<PathBuf, (String, SessionFileMeta)>,
    last_cleanup: Option<Instant>,
    /// Timestamped batch name used for trash / conflict backups during the current sync.
    batch: String,
}

/// Result of reconciling local Claude projects with the shared registry (not yet persisted).
struct Reconciled {
    registry: Registry,
    registry_dirty: bool,
    config: AppConfig,
    config_dirty: bool,
    auto_mapped: usize,
    local_counts: BTreeMap<String, usize>,
}

fn roots_identity(cfg: &AppConfig) -> String {
    let sync = cfg
        .sync_dir
        .as_ref()
        .map(|p| normalize_windows_path(&p.to_string_lossy()).to_lowercase())
        .unwrap_or_default();
    let claude = normalize_windows_path(&cfg.claude_dir.to_string_lossy()).to_lowercase();
    format!("{}|{}", sync, claude)
}

fn new_batch() -> String {
    Utc::now().format(BATCH_FORMAT).to_string()
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

fn rel_to_path(rel: &str) -> PathBuf {
    rel.split('/').filter(|s| !s.is_empty() && *s != "..").collect()
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).to_string())
}

fn preview_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(PREVIEW_LIMIT)
        .collect()
}

fn activity(report: &mut SyncReport, kind: &str, path: &str, detail: impl Into<String>) {
    report.activities.push(ActivityEntry {
        timestamp: Utc::now(),
        kind: kind.to_string(),
        path: path.to_string(),
        detail: detail.into(),
    });
}

struct ProgressReporter<'a> {
    callback: &'a mut dyn FnMut(&SyncProgress),
    progress: SyncProgress,
    batch_start: usize,
}

impl ProgressReporter<'_> {
    fn emit(&mut self, report: &SyncReport) {
        self.progress.recent = report.activities.iter().rev().take(5).cloned().collect();
        (self.callback)(&self.progress);
    }

    fn scanning(&mut self, phase: String, report: &SyncReport) {
        self.batch_start = self.progress.checked;
        self.progress.phase = phase;
        self.progress.current_item = None;
        self.progress.completed = 0;
        self.progress.total = 0;
        self.progress.upcoming.clear();
        self.emit(report);
    }

    fn checking(&mut self, items: &[String], index: usize, report: &SyncReport) {
        self.progress.current_item = Some(items[index].clone());
        self.progress.completed = index;
        self.progress.total = items.len();
        self.progress.checked = self.batch_start + index;
        self.progress.upcoming = items.iter().skip(index + 1).take(4).cloned().collect();
        self.emit(report);
    }

    fn finished(&mut self, total: usize, report: &SyncReport) {
        self.progress.current_item = None;
        self.progress.completed = total;
        self.progress.total = total;
        self.progress.checked = self.batch_start + total;
        self.progress.upcoming.clear();
        self.emit(report);
    }
}

/// True if both sides still match the fingerprints recorded at the last successful sync.
fn unchanged_since_last_sync(
    state: &Option<FileState>,
    local_path: &str,
    local_fp: &Option<String>,
    remote_fp: &Option<String>,
) -> bool {
    matches!(state, Some(s)
        if s.local_path.as_deref() == Some(local_path)
            && local_fp.is_some()
            && remote_fp.is_some()
            && &s.local_fp == local_fp
            && &s.remote_fp == remote_fp)
}

/// Last-synced hash, but only if it was recorded against the same local file.
fn tracked_hash<'a>(state: &'a Option<FileState>, local_path: &str) -> Option<&'a str> {
    state
        .as_ref()
        .filter(|s| s.local_path.as_deref() == Some(local_path))
        .map(|s| s.hash.as_str())
}

/// Writes a local file only if it hasn't changed since it was read (`expected` fingerprint;
/// `None` means it must still be missing). Returns false if the write was skipped.
fn write_local_if_unchanged(path: &Path, bytes: &[u8], expected: &Option<String>) -> Result<bool> {
    if local_fingerprint(path) != *expected {
        return Ok(false);
    }
    atomic_write(path, bytes)?;
    Ok(true)
}

impl SyncEngine {
    pub fn new(app_data_dir: PathBuf, config: AppConfig) -> Result<Self> {
        let db = StateDb::open(&app_data_dir)?;
        db.ensure_roots(&roots_identity(&config))?;
        Ok(Self {
            app_data_dir,
            config,
            db,
            home_dir_override: None,
            session_meta_cache: HashMap::new(),
            last_cleanup: None,
            batch: new_batch(),
        })
    }

    pub fn with_home_dir(mut self, home_dir: PathBuf) -> Self {
        self.home_dir_override = Some(normalize_windows_path(&home_dir.to_string_lossy()));
        self
    }

    fn effective_home_dir(&self) -> Option<String> {
        self.home_dir_override.clone().or_else(default_home_dir)
    }

    fn rewrite_ctx(&self, project_root: Option<&str>) -> PathRewriteContext {
        let claude_str = self.config.claude_dir.to_string_lossy().to_string();
        let home = self.effective_home_dir();
        PathRewriteContext::new(project_root, &claude_str, home.as_deref())
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn update_config(&mut self, config: AppConfig) -> Result<()> {
        save_config(&self.app_data_dir, &config)?;
        self.db.ensure_roots(&roots_identity(&config))?;
        self.config = config;
        Ok(())
    }

    pub fn recent_activities(&self, limit: usize) -> Result<Vec<ActivityEntry>> {
        self.db.recent_activities(limit)
    }

    pub fn pending_approvals(&self) -> Result<Vec<PendingApproval>> {
        self.db.pending_approvals()
    }

    /// Approves or rejects one specific incoming version of a gated file.
    /// Approved changes are applied on the next sync; rejected ones are ignored until they change again.
    pub fn resolve_approval(&self, remote_key: &str, remote_hash: &str, approve: bool) -> Result<()> {
        self.db.set_approval_decision(remote_key, remote_hash, approve)
    }

    fn provider(&self) -> Result<LocalFolderProvider> {
        let sync_dir = self.config.sync_dir.clone().ok_or_else(|| {
            SessionShareError::Config("Synced drive folder is not configured yet.".to_string())
        })?;
        LocalFolderProvider::new(sync_dir)
    }

    fn trash_dir(&self) -> PathBuf {
        self.app_data_dir.join("trash")
    }

    fn conflicts_dir(&self) -> PathBuf {
        self.app_data_dir.join("conflicts")
    }

    fn load_registry(&self, provider: &dyn StorageProvider) -> Result<Registry> {
        match provider.read_bytes(REGISTRY_PATH)? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
            None => Ok(Registry {
                version: 1,
                projects: BTreeMap::new(),
            }),
        }
    }

    fn save_registry(&self, provider: &dyn StorageProvider, registry: &Registry) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(registry)?;
        provider.write_bytes(REGISTRY_PATH, &bytes)
    }

    fn load_machine_manifests(
        &self,
        provider: &dyn StorageProvider,
    ) -> Result<Vec<MachineManifest>> {
        let files = provider.list_files("machines")?;
        let mut manifests = Vec::new();
        for f in files {
            if !f.ends_with(".json") {
                continue;
            }
            if let Some(bytes) = provider.read_bytes(&f)? {
                if let Ok(m) = serde_json::from_slice::<MachineManifest>(&bytes) {
                    manifests.push(m);
                }
            }
        }
        Ok(manifests)
    }

    /// Writes this PC's manifest only when its content changed, so syncs don't keep
    /// modifying the synced drive (which would make every PC's watcher fire again).
    fn save_own_manifest(&self, provider: &dyn StorageProvider) -> Result<()> {
        let path = format!("machines/{}.json", self.config.machine_id);
        if let Some(bytes) = provider.read_bytes(&path)? {
            if let Ok(existing) = serde_json::from_slice::<MachineManifest>(&bytes) {
                if existing.machine_name == self.config.machine_name
                    && existing.project_paths == self.config.local_project_paths
                {
                    return Ok(());
                }
            }
        }
        let manifest = MachineManifest {
            machine_id: self.config.machine_id.clone(),
            machine_name: self.config.machine_name.clone(),
            updated_at: Utc::now(),
            project_paths: self.config.local_project_paths.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        provider.write_bytes(&path, &bytes)
    }

    /// Builds the list of workspace roots to search when auto-resolving unmapped remote projects:
    /// combines explicitly configured `workspace_roots` with parent directories of already-known projects.
    fn collect_search_roots(config: &AppConfig) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        let mut roots = Vec::new();

        for r in &config.workspace_roots {
            let norm = normalize_windows_path(r);
            if !norm.is_empty() && seen.insert(norm.to_ascii_lowercase()) {
                roots.push(PathBuf::from(norm));
            }
        }

        // Infer sibling workspace roots from already-mapped local projects
        for local_path in config.local_project_paths.values() {
            if let Some(parent) = Path::new(local_path).parent() {
                let p_str = normalize_windows_path(&parent.to_string_lossy());
                if p_str.len() > 3 && seen.insert(p_str.to_ascii_lowercase()) {
                    roots.push(PathBuf::from(p_str));
                }
            }
        }

        roots
    }

    /// Discovers local Claude projects and reconciles them with `registry.json` without writing
    /// anything (safe for UI listing). Use [`SyncEngine::sync_all`] to persist the result.
    pub fn discover_projects(&mut self) -> Result<Vec<ProjectView>> {
        let provider = self.provider()?;
        let (views, _) = self.discover_projects_internal(&provider, false)?;
        Ok(views)
    }

    /// Lists projects and sessions with a single (read-only) discovery pass.
    pub fn overview(&mut self) -> Result<(Vec<ProjectView>, Vec<SessionSummary>)> {
        let provider = self.provider()?;
        let (projects, _) = self.discover_projects_internal(&provider, false)?;
        let sessions = self.sessions_for(&provider, &projects)?;
        Ok((projects, sessions))
    }

    fn reconcile(&self, provider: &dyn StorageProvider) -> Result<Reconciled> {
        let mut registry = self.load_registry(provider)?;
        let mut config = self.config.clone();
        let local_projects = scan_local_projects(&config.claude_dir)?;
        let home_dir = self.effective_home_dir();
        let mut registry_dirty = false;
        let mut config_dirty = false;
        let mut auto_mapped = 0usize;

        // Index existing local_project_paths by normalized path -> canonical_id
        let mut local_path_to_id: BTreeMap<String, String> = BTreeMap::new();
        for (cid, path) in &config.local_project_paths {
            local_path_to_id.insert(normalize_windows_path(path).to_ascii_lowercase(), cid.clone());
        }

        // Index registry by normalized git remote
        let mut git_to_id: BTreeMap<String, String> = BTreeMap::new();
        for (cid, proj) in &registry.projects {
            if let Some(remote) = &proj.git_remote_normalized {
                git_to_id.insert(remote.clone(), cid.clone());
            }
        }

        let mut local_counts: BTreeMap<String, usize> = BTreeMap::new();

        for lp in &local_projects {
            let local_path = match &lp.detected_local_path {
                Some(p) => p.clone(),
                None => continue,
            };
            let norm_key = normalize_windows_path(&local_path).to_ascii_lowercase();
            let home_hint = compute_home_relative_hint(&local_path, home_dir.as_deref());

            // 1. Existing mapping in config
            let (canonical_id, source) = if let Some(cid) = local_path_to_id.get(&norm_key) {
                (
                    cid.clone(),
                    config
                        .mapping_sources
                        .get(cid)
                        .cloned()
                        .unwrap_or_else(|| "local".to_string()),
                )
            } else if let Some(remote) = &lp.git_remote_normalized {
                // 2. Auto-match via Git remote
                if let Some(existing) = git_to_id.get(remote) {
                    (existing.clone(), "git".to_string())
                } else {
                    (canonical_id_from_git(remote), "git".to_string())
                }
            } else {
                // 3. Fallback deterministic ID from display name
                (
                    canonical_id_from_name(&display_name_from_path(&local_path)),
                    "folder-name".to_string(),
                )
            };

            if let Some(existing_proj) = registry.projects.get_mut(&canonical_id) {
                if existing_proj.home_relative_hint.is_none() && home_hint.is_some() {
                    existing_proj.home_relative_hint = home_hint;
                    existing_proj.updated_at = Utc::now();
                    registry_dirty = true;
                }
            } else {
                let now = Utc::now();
                registry.projects.insert(
                    canonical_id.clone(),
                    CanonicalProject {
                        id: canonical_id.clone(),
                        display_name: display_name_from_path(&local_path),
                        git_remote_normalized: lp.git_remote_normalized.clone(),
                        home_relative_hint: home_hint,
                        created_at: now,
                        updated_at: now,
                    },
                );
                if let Some(remote) = &lp.git_remote_normalized {
                    git_to_id.insert(remote.clone(), canonical_id.clone());
                }
                registry_dirty = true;
            }

            if config.local_project_paths.get(&canonical_id) != Some(&local_path) {
                config
                    .local_project_paths
                    .insert(canonical_id.clone(), local_path.clone());
                config.mapping_sources.insert(canonical_id.clone(), source);
                local_path_to_id.insert(norm_key, canonical_id.clone());
                config_dirty = true;
            }

            if !config.project_sync_enabled.contains_key(&canonical_id) {
                config
                    .project_sync_enabled
                    .insert(canonical_id.clone(), true);
                config_dirty = true;
            }

            local_counts.insert(canonical_id, lp.session_files.len());
        }

        // Smart Auto-Resolver: for any remote project not yet mapped on this PC,
        // automatically search %USERPROFILE%-relative paths and workspace roots.
        let search_roots = Self::collect_search_roots(&config);
        for (cid, proj) in &registry.projects {
            if config.local_project_paths.contains_key(cid) {
                continue;
            }
            if let Some((resolved_path, source)) =
                auto_resolve_unmapped_project(proj, home_dir.as_deref(), &search_roots)
            {
                let norm_key = resolved_path.to_ascii_lowercase();
                // Avoid colliding with another already-mapped project at the exact same path
                if !local_path_to_id.contains_key(&norm_key) {
                    config
                        .local_project_paths
                        .insert(cid.clone(), resolved_path.clone());
                    config
                        .mapping_sources
                        .insert(cid.clone(), source.to_string());
                    config
                        .project_sync_enabled
                        .entry(cid.clone())
                        .or_insert(true);
                    local_path_to_id.insert(norm_key, cid.clone());
                    config_dirty = true;
                    auto_mapped += 1;
                }
            }
        }

        Ok(Reconciled {
            registry,
            registry_dirty,
            config,
            config_dirty,
            auto_mapped,
            local_counts,
        })
    }

    fn discover_projects_internal(
        &mut self,
        provider: &LocalFolderProvider,
        persist: bool,
    ) -> Result<(Vec<ProjectView>, usize)> {
        let r = self.reconcile(provider)?;

        if persist {
            if r.registry_dirty {
                self.save_registry(provider, &r.registry)?;
            }
            if r.config_dirty {
                save_config(&self.app_data_dir, &r.config)?;
                self.config = r.config.clone();
            }
            self.save_own_manifest(provider)?;
        }
        let config = &r.config;

        let manifests = self.load_machine_manifests(provider)?;

        let mut views = Vec::new();
        for (cid, proj) in &r.registry.projects {
            let local_path = config.local_project_paths.get(cid).cloned();
            let encoded_local_dir = local_path.as_deref().map(|p| {
                resolve_encoded_project_dir(&config.claude_dir, p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| encode_project_path(p))
            });
            let mapping_source = config.mapping_sources.get(cid).cloned();

            let remote_machines: Vec<String> = manifests
                .iter()
                .filter(|m| m.machine_id != config.machine_id && m.project_paths.contains_key(cid))
                .map(|m| m.machine_name.clone())
                .collect();

            let remote_prefix = format!("projects/{}/sessions", cid);
            let remote_session_count = provider
                .list_files(&remote_prefix)?
                .into_iter()
                .filter(|f| {
                    let rel = f.strip_prefix(&format!("{}/", remote_prefix)).unwrap_or(f);
                    !rel.contains('/') && rel.ends_with(".jsonl")
                })
                .count();

            let sync_enabled = config.project_sync_enabled.get(cid).copied().unwrap_or(false);

            views.push(ProjectView {
                canonical_id: cid.clone(),
                display_name: proj.display_name.clone(),
                git_remote: proj.git_remote_normalized.clone(),
                needs_mapping: local_path.is_none(),
                local_path,
                encoded_local_dir,
                mapping_source,
                remote_machines,
                sync_enabled,
                local_session_count: r.local_counts.get(cid).copied().unwrap_or(0),
                remote_session_count,
            });
        }

        views.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        });
        Ok((views, r.auto_mapped))
    }

    /// Manually maps a canonical remote project to a local Windows folder on this PC.
    pub fn map_project_local_path(&mut self, canonical_id: &str, local_path: &str) -> Result<()> {
        let normalized = normalize_windows_path(local_path);
        self.config
            .local_project_paths
            .insert(canonical_id.to_string(), normalized);
        self.config
            .mapping_sources
            .insert(canonical_id.to_string(), "manual".to_string());
        self.config
            .project_sync_enabled
            .entry(canonical_id.to_string())
            .or_insert(true);
        save_config(&self.app_data_dir, &self.config)?;
        if let Ok(provider) = self.provider() {
            let _ = self.save_own_manifest(&provider);
        }
        Ok(())
    }

    /// Toggles sync on/off for a specific project.
    pub fn set_project_sync_enabled(&mut self, canonical_id: &str, enabled: bool) -> Result<()> {
        self.config
            .project_sync_enabled
            .insert(canonical_id.to_string(), enabled);
        save_config(&self.app_data_dir, &self.config)
    }

    /// Lists all sessions across local and remote synced projects (read-only).
    pub fn list_sessions(&mut self) -> Result<Vec<SessionSummary>> {
        Ok(self.overview()?.1)
    }

    fn session_meta(&mut self, path: &Path) -> SessionFileMeta {
        let fp = local_fingerprint(path).unwrap_or_default();
        if let Some((cached_fp, meta)) = self.session_meta_cache.get(path) {
            if !fp.is_empty() && *cached_fp == fp {
                return meta.clone();
            }
        }
        let meta = inspect_session_file(path);
        self.session_meta_cache
            .insert(path.to_path_buf(), (fp, meta.clone()));
        meta
    }

    fn sessions_for(
        &mut self,
        provider: &LocalFolderProvider,
        projects: &[ProjectView],
    ) -> Result<Vec<SessionSummary>> {
        let mut sessions = Vec::new();

        for proj in projects {
            let mut seen_ids: BTreeSet<String> = BTreeSet::new();
            let local_dir = proj
                .encoded_local_dir
                .as_ref()
                .map(|enc| self.config.claude_dir.join("projects").join(enc));

            let remote_prefix = format!("projects/{}/sessions", proj.canonical_id);
            let remote_files: BTreeSet<String> = provider
                .list_files(&remote_prefix)?
                .into_iter()
                .filter_map(|f| {
                    let rel = f.strip_prefix(&format!("{}/", remote_prefix))?;
                    if !rel.contains('/') && rel.ends_with(".jsonl") {
                        Some(rel.trim_end_matches(".jsonl").to_string())
                    } else {
                        None
                    }
                })
                .collect();

            let mut local_files = Vec::new();
            if let Some(dir) = &local_dir {
                if let Ok(rd) = fs::read_dir(dir) {
                    for entry in rd.flatten() {
                        let p = entry.path();
                        if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                            local_files.push(p);
                        }
                    }
                }
            }

            for p in local_files {
                let Some(sid) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                    continue;
                };
                let meta = self.session_meta(&p);
                seen_ids.insert(sid.clone());
                sessions.push(SessionSummary {
                    in_remote: remote_files.contains(&sid),
                    in_local: true,
                    session_id: sid,
                    project_id: proj.canonical_id.clone(),
                    project_name: proj.display_name.clone(),
                    local_project_path: proj.local_path.clone(),
                    title: meta.title,
                    last_modified: meta.last_modified,
                    line_count: meta.line_count,
                    size_bytes: meta.size_bytes,
                });
            }

            // Remote-only sessions (not yet pulled or project not yet mapped)
            for sid in remote_files {
                if seen_ids.contains(&sid) {
                    continue;
                }
                let remote_file = provider
                    .root()
                    .join(rel_to_path(&remote_prefix))
                    .join(format!("{}.jsonl", sid));
                let meta = self.session_meta(&remote_file);
                sessions.push(SessionSummary {
                    session_id: sid,
                    project_id: proj.canonical_id.clone(),
                    project_name: proj.display_name.clone(),
                    local_project_path: proj.local_path.clone(),
                    title: meta.title,
                    last_modified: meta.last_modified,
                    line_count: meta.line_count,
                    size_bytes: meta.size_bytes,
                    in_local: false,
                    in_remote: true,
                });
            }
        }

        sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
        Ok(sessions)
    }

    /// Performs a full bidirectional sync across all enabled projects and global Claude assets.
    pub fn sync_all(&mut self) -> Result<SyncReport> {
        self.sync_all_with_progress(|_| {})
    }

    /// Performs a sync and reports file-checking progress as each scope is discovered.
    pub fn sync_all_with_progress(
        &mut self,
        mut on_progress: impl FnMut(&SyncProgress),
    ) -> Result<SyncReport> {
        let mut report = SyncReport {
            timestamp: Utc::now(),
            ..Default::default()
        };
        let mut progress = ProgressReporter {
            callback: &mut on_progress,
            progress: SyncProgress::default(),
            batch_start: 0,
        };
        progress.scanning("Finding projects".to_string(), &report);
        self.batch = new_batch();

        // Ensure base ~/.claude directory exists even on a fresh PC
        let _ = fs::create_dir_all(&self.config.claude_dir);

        let provider = self.provider()?;
        let (projects, auto_mapped) = self.discover_projects_internal(&provider, true)?;
        report.auto_mapped = auto_mapped;
        if auto_mapped > 0 {
            activity(
                &mut report,
                "auto-map",
                REGISTRY_PATH,
                format!(
                    "Automatically resolved {} project folder(s) on this PC",
                    auto_mapped
                ),
            );
        }

        // 1. Sync enabled & mapped projects
        for proj in &projects {
            if !proj.sync_enabled {
                continue;
            }
            let Some(local_root) = &proj.local_path else {
                continue;
            };
            let local_claude_proj_dir =
                resolve_encoded_project_dir(&self.config.claude_dir, local_root);

            if let Err(e) = self.sync_project_sessions(
                &provider,
                &proj.canonical_id,
                local_root,
                &local_claude_proj_dir,
                &format!("{} · sessions", proj.display_name),
                &mut report,
                &mut progress,
            ) {
                report
                    .errors
                    .push(format!("Project {} sessions: {}", proj.display_name, e));
            }

            if self.config.global_options.project_memory {
                let local_mem = local_claude_proj_dir.join("memory");
                let remote_mem = format!("projects/{}/memory", proj.canonical_id);
                if let Err(e) =
                    self.sync_generic_dir(&provider, &local_mem, &remote_mem, &mut report, &mut progress)
                {
                    report
                        .errors
                        .push(format!("Project {} memory: {}", proj.display_name, e));
                }
            }
        }

        // 2. Sync global directories (missing dirs on either PC are handled seamlessly)
        let opts = self.config.global_options.clone();
        let global_dirs: [(&str, bool); 6] = [
            ("skills", opts.skills),
            ("agents", opts.agents),
            ("commands", opts.commands),
            ("plans", opts.plans),
            ("rules", opts.rules),
            ("hooks", opts.hooks),
        ];
        for (dir_name, enabled) in global_dirs {
            if enabled {
                if let Err(e) = self.sync_generic_dir(
                    &provider,
                    &self.config.claude_dir.join(dir_name),
                    &format!("global/{}", dir_name),
                    &mut report,
                    &mut progress,
                ) {
                    report.errors.push(format!("{}: {}", dir_name, e));
                }
            }
        }

        // 3. Sync global single files
        if opts.claude_md {
            let items = vec!["global/CLAUDE.md".to_string()];
            progress.scanning("Global files".to_string(), &report);
            progress.checking(&items, 0, &report);
            let result = self.sync_generic_file(
                &provider,
                &self.config.claude_dir.join("CLAUDE.md"),
                "global/CLAUDE.md",
                self.config.propagate_deletions,
                &mut report,
            );
            if let Err(e) = result {
                report.errors.push(format!("CLAUDE.md: {}", e));
            } else {
                progress.finished(1, &report);
            }
        }
        if opts.settings_json {
            let items = vec![SETTINGS_KEY.to_string()];
            progress.scanning("Global files".to_string(), &report);
            progress.checking(&items, 0, &report);
            if let Err(e) = self.sync_settings_json(&provider, &mut report) {
                report.errors.push(format!("settings.json: {}", e));
            } else {
                progress.finished(1, &report);
            }
        }
        if opts.history_jsonl {
            let items = vec![HISTORY_KEY.to_string()];
            progress.scanning("Global files".to_string(), &report);
            progress.checking(&items, 0, &report);
            if let Err(e) = self.sync_history_jsonl(&provider, &mut report) {
                report.errors.push(format!("history.jsonl: {}", e));
            } else {
                progress.finished(1, &report);
            }
        }

        progress.scanning("Finishing sync".to_string(), &report);
        report.pending_approvals = self.db.pending_approvals()?.len();
        report.unresolved_conflicts = self.db.conflicts()?.len();
        self.maybe_cleanup(&provider);

        // Persist activities
        for act in &report.activities {
            let _ = self.db.record_activity(act);
        }

        Ok(report)
    }

    fn record(
        &self,
        provider: &dyn StorageProvider,
        key: &str,
        hash: &str,
        local_path: &Path,
    ) -> Result<()> {
        self.db.set_state(
            key,
            &FileState {
                hash: hash.to_string(),
                local_path: Some(path_str(local_path)),
                local_fp: local_fingerprint(local_path),
                remote_fp: provider.fingerprint(key),
            },
        )
    }

    /// Records state without a local fingerprint so the next sync re-compares the file
    /// (used when a local write was postponed).
    fn record_deferred(
        &self,
        provider: &dyn StorageProvider,
        key: &str,
        hash: &str,
        local_path: &Path,
    ) -> Result<()> {
        self.db.set_state(
            key,
            &FileState {
                hash: hash.to_string(),
                local_path: Some(path_str(local_path)),
                local_fp: None,
                remote_fp: provider.fingerprint(key),
            },
        )
    }

    fn trash_local_file(&self, local_path: &Path, key: &str, report: &mut SyncReport) -> Result<()> {
        let dest = self.trash_dir().join(&self.batch).join(rel_to_path(key));
        move_file(local_path, &dest)?;
        self.db.delete_state(key)?;
        self.db.delete_base(key)?;
        self.db.clear_conflict(key)?;
        report.deleted += 1;
        activity(
            report,
            "delete",
            key,
            format!(
                "Deleted on another PC; local copy moved to {}",
                dest.display()
            ),
        );
        Ok(())
    }

    fn trash_remote_file(
        &self,
        provider: &dyn StorageProvider,
        key: &str,
        report: &mut SyncReport,
    ) -> Result<()> {
        provider.trash(key, &self.batch)?;
        self.db.delete_state(key)?;
        self.db.delete_base(key)?;
        self.db.clear_conflict(key)?;
        report.deleted += 1;
        activity(
            report,
            "delete",
            key,
            format!(
                "Deleted on this PC; synced-drive copy moved to {}/{}",
                REMOTE_TRASH_DIR, self.batch
            ),
        );
        Ok(())
    }

    fn backup_conflict(&self, key: &str, bytes: &[u8]) -> Result<PathBuf> {
        let dest = self.conflicts_dir().join(&self.batch).join(rel_to_path(key));
        atomic_write(&dest, bytes)?;
        Ok(dest)
    }

    /// Decides whether deletions may propagate for one folder, refusing when a large share of
    /// tracked files vanished at once (e.g. a folder was moved or a drive is not fully synced).
    fn deletions_allowed(
        &self,
        scope: &str,
        local_set: &BTreeSet<String>,
        remote_set: &BTreeSet<String>,
        key_of: &dyn Fn(&str) -> String,
        report: &mut SyncReport,
    ) -> bool {
        if !self.config.propagate_deletions {
            return false;
        }
        let total = local_set.union(remote_set).count();
        let candidates = local_set
            .symmetric_difference(remote_set)
            .filter(|rel| matches!(self.db.get_state(&key_of(rel)), Ok(Some(_))))
            .count();
        if candidates > MASS_DELETE_MIN && candidates * 2 >= total {
            report.errors.push(format!(
                "{}: {} previously synced files are missing on one side; they were restored instead of deleted (safety guard).",
                scope, candidates
            ));
            return false;
        }
        true
    }

    /// Returns true if an incoming gated change may be applied; otherwise records it for review.
    fn approval_gate(&self, key: &str, hash: &str, summary: &str, preview: &str) -> Result<bool> {
        if !self.config.require_hook_approval {
            return Ok(true);
        }
        match self.db.approval_decision(key, hash)? {
            Some(true) => {
                self.db.clear_pending(key)?;
                Ok(true)
            }
            Some(false) => Ok(false),
            None => {
                self.db.upsert_pending(&PendingApproval {
                    remote_key: key.to_string(),
                    remote_hash: hash.to_string(),
                    detected_at: Utc::now(),
                    summary: summary.to_string(),
                    preview: preview.chars().take(PREVIEW_LIMIT).collect(),
                })?;
                Ok(false)
            }
        }
    }

    fn sync_project_sessions(
        &self,
        provider: &dyn StorageProvider,
        canonical_id: &str,
        local_root: &str,
        local_claude_proj_dir: &Path,
        scope: &str,
        report: &mut SyncReport,
        progress: &mut ProgressReporter<'_>,
    ) -> Result<()> {
        progress.scanning(format!("Finding files · {}", scope), report);
        let remote_prefix = format!("projects/{}/sessions", canonical_id);
        let ctx = self.rewrite_ctx(Some(local_root));
        let grace = self.config.active_session_grace_secs;

        // Gather all local .jsonl files recursively (main transcripts + subagent transcripts)
        let mut local_set: BTreeSet<String> = BTreeSet::new();
        if local_claude_proj_dir.is_dir() {
            for entry in WalkDir::new(local_claude_proj_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let name = entry.file_name().to_string_lossy();
                if name.ends_with(TMP_SUFFIX) || name.contains(".conflict.") {
                    continue;
                }
                if let Ok(rel) = p.strip_prefix(local_claude_proj_dir) {
                    let rel_posix = rel.to_string_lossy().replace('\\', "/");
                    if !rel_posix.starts_with("memory/") {
                        local_set.insert(rel_posix);
                    }
                }
            }
        }

        let mut remote_set: BTreeSet<String> = BTreeSet::new();
        for remote_full in provider.list_files(&remote_prefix)? {
            if let Some(rel) = remote_full.strip_prefix(&format!("{}/", remote_prefix)) {
                if rel.ends_with(".jsonl") && !rel.contains(".conflict.") {
                    remote_set.insert(rel.to_string());
                }
            }
        }

        let key_of = |rel: &str| format!("{}/{}", remote_prefix, rel);
        let allow_deletes = self.deletions_allowed(
            &format!("Project {} sessions", canonical_id),
            &local_set,
            &remote_set,
            &key_of,
            report,
        );
        let all: Vec<String> = local_set.union(&remote_set).cloned().collect();
        let items: Vec<String> = all.iter().map(|rel| key_of(rel)).collect();
        let mut pulled_any = false;

        progress.scanning(scope.to_string(), report);
        for (index, rel) in all.into_iter().enumerate() {
            progress.checking(&items, index, report);
            let local_path = local_claude_proj_dir.join(rel_to_path(&rel));
            let local_path_s = path_str(&local_path);
            let remote_key = key_of(&rel);
            let state = self.db.get_state(&remote_key)?;
            let local_fp = local_fingerprint(&local_path);
            let remote_fp = provider.fingerprint(&remote_key);
            if unchanged_since_last_sync(&state, &local_path_s, &local_fp, &remote_fp) {
                continue;
            }
            progress.finished(items.len(), report);
            let last_hash = tracked_hash(&state, &local_path_s);

            let local_str = match local_fp {
                Some(_) => Some(lossy(read_with_retry(&local_path)?)),
                None => None,
            };
            let remote_str = provider.read_bytes(&remote_key)?.map(lossy);

            match (local_str, remote_str) {
                (Some(local_raw), None) => {
                    let portable = normalize_jsonl_with_ctx(&local_raw, &ctx);
                    let h = sha256_hex(portable.as_bytes());
                    if allow_deletes
                        && last_hash == Some(h.as_str())
                        && !modified_within(&local_path, grace)
                    {
                        self.trash_local_file(&local_path, &remote_key, report)?;
                        continue;
                    }
                    provider.write_bytes(&remote_key, portable.as_bytes())?;
                    self.record(provider, &remote_key, &h, &local_path)?;
                    report.pushed += 1;
                    activity(report, "push", &remote_key, "Uploaded new session transcript");
                }
                (None, Some(remote_portable)) => {
                    let h = sha256_hex(remote_portable.as_bytes());
                    if allow_deletes && last_hash == Some(h.as_str()) {
                        self.trash_remote_file(provider, &remote_key, report)?;
                        continue;
                    }
                    let local_restored = denormalize_jsonl_with_ctx(&remote_portable, &ctx);
                    if !write_local_if_unchanged(&local_path, local_restored.as_bytes(), &None)? {
                        report.deferred += 1;
                        continue;
                    }
                    self.record(provider, &remote_key, &h, &local_path)?;
                    report.pulled += 1;
                    pulled_any = true;
                    activity(
                        report,
                        "pull",
                        &remote_key,
                        "Downloaded session transcript with path remapping",
                    );
                }
                (Some(local_raw), Some(remote_portable)) => {
                    let local_portable = normalize_jsonl_with_ctx(&local_raw, &ctx);
                    // Never rewrite a transcript Claude Code may still be appending to.
                    let active = modified_within(&local_path, grace);
                    match merge_jsonl(&local_portable, &remote_portable) {
                        MergeOutcome::Identical => {
                            let h = sha256_hex(remote_portable.as_bytes());
                            self.record(provider, &remote_key, &h, &local_path)?;
                        }
                        MergeOutcome::LocalAhead(merged_portable) => {
                            provider.write_bytes(&remote_key, merged_portable.as_bytes())?;
                            let h = sha256_hex(merged_portable.as_bytes());
                            self.record(provider, &remote_key, &h, &local_path)?;
                            report.pushed += 1;
                            activity(report, "push", &remote_key, "Pushed new session messages");
                        }
                        MergeOutcome::RemoteAhead(merged_portable) => {
                            let h = sha256_hex(merged_portable.as_bytes());
                            let local_restored = denormalize_jsonl_with_ctx(&merged_portable, &ctx);
                            if active
                                || !write_local_if_unchanged(
                                    &local_path,
                                    local_restored.as_bytes(),
                                    &local_fp,
                                )?
                            {
                                self.record_deferred(provider, &remote_key, &h, &local_path)?;
                                report.deferred += 1;
                                continue;
                            }
                            self.record(provider, &remote_key, &h, &local_path)?;
                            report.pulled += 1;
                            pulled_any = true;
                            activity(report, "pull", &remote_key, "Pulled new session messages");
                        }
                        MergeOutcome::Merged {
                            content,
                            local_only_count,
                            remote_only_count,
                        } => {
                            provider.write_bytes(&remote_key, content.as_bytes())?;
                            let h = sha256_hex(content.as_bytes());
                            let local_restored = denormalize_jsonl_with_ctx(&content, &ctx);
                            if active
                                || !write_local_if_unchanged(
                                    &local_path,
                                    local_restored.as_bytes(),
                                    &local_fp,
                                )?
                            {
                                self.record_deferred(provider, &remote_key, &h, &local_path)?;
                                report.pushed += 1;
                                report.deferred += 1;
                                activity(
                                    report,
                                    "push",
                                    &remote_key,
                                    format!(
                                        "Pushed {} new local lines; pulling {} remote lines is deferred while the session is active",
                                        local_only_count, remote_only_count
                                    ),
                                );
                                continue;
                            }
                            self.record(provider, &remote_key, &h, &local_path)?;
                            report.merged += 1;
                            pulled_any = true;
                            activity(
                                report,
                                "merge",
                                &remote_key,
                                format!(
                                    "Merged {} local + {} remote lines",
                                    local_only_count, remote_only_count
                                ),
                            );
                        }
                    }
                }
                (None, None) => {
                    if state.is_some() {
                        self.db.delete_state(&remote_key)?;
                    }
                }
            }
        }

        // Remove stale sessions-index.json if we pulled new sessions so Claude Code re-indexes cleanly
        if pulled_any {
            let idx = local_claude_proj_dir.join("sessions-index.json");
            if idx.is_file() {
                let _ = fs::remove_file(idx);
            }
        }

        Ok(())
    }

    /// Checks whether an incoming `settings.json` may be applied: changes to hooks, permissions or
    /// command helpers require approval.
    fn settings_pull_allowed(
        &self,
        local_raw: Option<&str>,
        incoming_local_form: &str,
        remote_hash: &str,
    ) -> Result<bool> {
        let incoming_sig = command_signature(Some(incoming_local_form));
        if incoming_sig == command_signature(local_raw) {
            return Ok(true);
        }
        self.approval_gate(
            SETTINGS_KEY,
            remote_hash,
            "settings.json from another PC changes hooks, permissions or command helpers",
            &incoming_sig,
        )
    }

    /// Syncs `settings.json` with automatic `${SESSIONSHARE_CLAUDE_DIR}` and `${SESSIONSHARE_HOME_DIR}`
    /// path rewriting. Keys in `settings_excluded_keys` (e.g. `env`) never leave this PC.
    fn sync_settings_json(
        &self,
        provider: &dyn StorageProvider,
        report: &mut SyncReport,
    ) -> Result<()> {
        let local_path = self.config.claude_dir.join("settings.json");
        let local_path_s = path_str(&local_path);
        let key = SETTINGS_KEY;
        let ctx = self.rewrite_ctx(None);
        let excluded = &self.config.settings_excluded_keys;

        let state = self.db.get_state(key)?;
        let local_fp = local_fingerprint(&local_path);
        if unchanged_since_last_sync(&state, &local_path_s, &local_fp, &provider.fingerprint(key)) {
            return Ok(());
        }
        let last_hash = tracked_hash(&state, &local_path_s);

        let local_str = match local_fp {
            Some(_) => Some(lossy(read_with_retry(&local_path)?)),
            None => None,
        };
        let mut remote_str = provider.read_bytes(key)?.map(lossy);
        // Earlier versions synced every key: scrub excluded keys (secrets) from the drive copy.
        if let Some(r) = &remote_str {
            if has_any_key(r, excluded) {
                let scrubbed = strip_keys(r, excluded);
                provider.write_bytes(key, scrubbed.as_bytes())?;
                remote_str = Some(scrubbed);
            }
        }

        let local_portable = local_str
            .as_deref()
            .map(|l| strip_keys(&normalize_settings_json(l, &ctx), excluded));
        let restore = |r: &str| {
            reinject_keys(
                &denormalize_settings_json(r, &ctx),
                local_str.as_deref(),
                excluded,
            )
        };

        match (local_portable, remote_str) {
            (Some(lp), None) => {
                let h = sha256_hex(lp.as_bytes());
                provider.write_bytes(key, lp.as_bytes())?;
                self.record_with_base(provider, key, &h, &local_path, lp.as_bytes())?;
                report.pushed += 1;
                activity(report, "push", key, "Uploaded settings.json (paths normalized, local-only keys excluded)");
            }
            (None, Some(r)) => {
                let h = sha256_hex(r.as_bytes());
                let restored = restore(&r);
                if !self.settings_pull_allowed(None, &restored, &h)? {
                    return Ok(());
                }
                if !write_local_if_unchanged(&local_path, restored.as_bytes(), &None)? {
                    report.deferred += 1;
                    return Ok(());
                }
                self.record_with_base(provider, key, &h, &local_path, r.as_bytes())?;
                report.pulled += 1;
                activity(report, "pull", key, "Downloaded settings.json (paths remapped)");
            }
            (Some(lp), Some(r)) => {
                let lh = sha256_hex(lp.as_bytes());
                let rh = sha256_hex(r.as_bytes());
                if lh == rh {
                    self.record_with_base(provider, key, &lh, &local_path, lp.as_bytes())?;
                    self.db.clear_pending(key)?;
                    return Ok(());
                }

                if last_hash == Some(rh.as_str()) {
                    provider.write_bytes(key, lp.as_bytes())?;
                    self.record_with_base(provider, key, &lh, &local_path, lp.as_bytes())?;
                    report.pushed += 1;
                    activity(report, "push", key, "Pushed updated settings.json");
                } else if last_hash == Some(lh.as_str()) {
                    let restored = restore(&r);
                    if !self.settings_pull_allowed(local_str.as_deref(), &restored, &rh)? {
                        return Ok(());
                    }
                    if !write_local_if_unchanged(&local_path, restored.as_bytes(), &local_fp)? {
                        report.deferred += 1;
                        return Ok(());
                    }
                    self.record_with_base(provider, key, &rh, &local_path, r.as_bytes())?;
                    report.pulled += 1;
                    activity(report, "pull", key, "Pulled updated settings.json (paths remapped)");
                } else {
                    // Both PCs changed settings.json: merge key by key against the last agreed version.
                    let base = last_hash.and_then(|h| self.base_text(key, h));
                    if let Some(merged) = base.as_deref().and_then(|b| json_merge3(b, &lp, &r)) {
                        let merged_local = restore(&merged);
                        let commands_unchanged = command_signature(Some(&merged_local))
                            == command_signature(local_str.as_deref());
                        // Merges that bring in hook / permission changes go through the merge view instead.
                        if commands_unchanged || !self.config.require_hook_approval {
                            if !write_local_if_unchanged(&local_path, merged_local.as_bytes(), &local_fp)? {
                                report.deferred += 1;
                                return Ok(());
                            }
                            provider.write_bytes(key, merged.as_bytes())?;
                            let h = sha256_hex(merged.as_bytes());
                            self.record_with_base(provider, key, &h, &local_path, merged.as_bytes())?;
                            report.merged += 1;
                            activity(report, "merge", key, "Merged settings.json changes from both PCs key by key");
                            return Ok(());
                        }
                    }
                    self.hold_conflict(key, &local_path, &lh, &rh, base.is_some(), report)?;
                }
            }
            (None, None) => {}
        }

        Ok(())
    }

    fn sync_history_jsonl(
        &self,
        provider: &dyn StorageProvider,
        report: &mut SyncReport,
    ) -> Result<()> {
        let local_path = self.config.claude_dir.join("history.jsonl");
        let key = HISTORY_KEY;
        let grace = self.config.active_session_grace_secs;

        let local_fp = local_fingerprint(&local_path);
        let local_str = match local_fp {
            Some(_) => Some(lossy(read_with_retry(&local_path)?)),
            None => None,
        };
        let remote_str = provider.read_bytes(key)?.map(lossy);

        let map = &self.config.local_project_paths;
        match (local_str, remote_str) {
            (Some(local_raw), None) => {
                let portable = normalize_history_jsonl(&local_raw, map);
                provider.write_bytes(key, portable.as_bytes())?;
                report.pushed += 1;
                activity(report, "push", key, "Uploaded global history.jsonl");
            }
            (None, Some(remote_portable)) => {
                let restored = denormalize_history_jsonl(&remote_portable, map);
                if !write_local_if_unchanged(&local_path, restored.as_bytes(), &None)? {
                    report.deferred += 1;
                    return Ok(());
                }
                report.pulled += 1;
                activity(report, "pull", key, "Downloaded global history.jsonl");
            }
            (Some(local_raw), Some(remote_portable)) => {
                let local_portable = normalize_history_jsonl(&local_raw, map);
                let active = modified_within(&local_path, grace);
                match merge_jsonl(&local_portable, &remote_portable) {
                    MergeOutcome::Identical => {}
                    MergeOutcome::LocalAhead(merged) => {
                        provider.write_bytes(key, merged.as_bytes())?;
                        report.pushed += 1;
                    }
                    MergeOutcome::RemoteAhead(merged) => {
                        let restored = denormalize_history_jsonl(&merged, map);
                        if active
                            || !write_local_if_unchanged(&local_path, restored.as_bytes(), &local_fp)?
                        {
                            report.deferred += 1;
                            return Ok(());
                        }
                        report.pulled += 1;
                    }
                    MergeOutcome::Merged { content, .. } => {
                        provider.write_bytes(key, content.as_bytes())?;
                        let restored = denormalize_history_jsonl(&content, map);
                        if active
                            || !write_local_if_unchanged(&local_path, restored.as_bytes(), &local_fp)?
                        {
                            report.pushed += 1;
                            report.deferred += 1;
                            return Ok(());
                        }
                        report.merged += 1;
                    }
                }
            }
            (None, None) => {}
        }

        Ok(())
    }

    fn sync_generic_dir(
        &self,
        provider: &dyn StorageProvider,
        local_dir: &Path,
        remote_prefix: &str,
        report: &mut SyncReport,
        progress: &mut ProgressReporter<'_>,
    ) -> Result<()> {
        progress.scanning(format!("Finding files · {}", remote_prefix), report);
        let mut local_set: BTreeSet<String> = BTreeSet::new();
        if local_dir.is_dir() {
            for entry in WalkDir::new(local_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy();
                if name.ends_with(TMP_SUFFIX) || name.contains(".conflict.") {
                    continue;
                }
                if let Ok(rel) = entry.path().strip_prefix(local_dir) {
                    local_set.insert(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }

        let mut remote_set: BTreeSet<String> = BTreeSet::new();
        for remote_full in provider.list_files(remote_prefix)? {
            if let Some(rel) = remote_full.strip_prefix(&format!("{}/", remote_prefix)) {
                if !rel.contains(".conflict.") {
                    remote_set.insert(rel.to_string());
                }
            }
        }

        let key_of = |rel: &str| format!("{}/{}", remote_prefix, rel);
        let allow_deletes =
            self.deletions_allowed(remote_prefix, &local_set, &remote_set, &key_of, report);

        let all: Vec<&String> = local_set.union(&remote_set).collect();
        let items: Vec<String> = all.iter().map(|rel| key_of(rel)).collect();
        progress.scanning(remote_prefix.to_string(), report);
        for (index, rel) in all.into_iter().enumerate() {
            progress.checking(&items, index, report);
            let local_file = local_dir.join(rel_to_path(rel));
            self.sync_generic_file(provider, &local_file, &key_of(rel), allow_deletes, report)?;
        }

        progress.finished(items.len(), report);
        Ok(())
    }

    fn sync_generic_file(
        &self,
        provider: &dyn StorageProvider,
        local_path: &Path,
        remote_key: &str,
        allow_deletes: bool,
        report: &mut SyncReport,
    ) -> Result<()> {
        let local_path_s = path_str(local_path);
        let state = self.db.get_state(remote_key)?;
        let local_fp = local_fingerprint(local_path);
        let remote_fp = provider.fingerprint(remote_key);
        if unchanged_since_last_sync(&state, &local_path_s, &local_fp, &remote_fp) {
            return Ok(());
        }
        let last_hash = tracked_hash(&state, &local_path_s);
        let gated = remote_key.starts_with(HOOKS_PREFIX);

        let local_bytes = match local_fp {
            Some(_) => Some(read_with_retry(local_path)?),
            None => None,
        };
        let remote_bytes = provider.read_bytes(remote_key)?;

        match (local_bytes, remote_bytes) {
            (Some(l), None) => {
                let h = sha256_hex(&l);
                if allow_deletes && last_hash == Some(h.as_str()) {
                    return self.trash_local_file(local_path, remote_key, report);
                }
                provider.write_bytes(remote_key, &l)?;
                self.record_with_base(provider, remote_key, &h, local_path, &l)?;
                report.pushed += 1;
                activity(report, "push", remote_key, "Uploaded file");
            }
            (None, Some(r)) => {
                let h = sha256_hex(&r);
                if allow_deletes && last_hash == Some(h.as_str()) {
                    return self.trash_remote_file(provider, remote_key, report);
                }
                if gated
                    && !self.approval_gate(
                        remote_key,
                        &h,
                        "New hook script from another PC",
                        &preview_text(&r),
                    )?
                {
                    return Ok(());
                }
                if !write_local_if_unchanged(local_path, &r, &None)? {
                    report.deferred += 1;
                    return Ok(());
                }
                self.record_with_base(provider, remote_key, &h, local_path, &r)?;
                report.pulled += 1;
                activity(report, "pull", remote_key, "Downloaded file");
            }
            (Some(l), Some(r)) => {
                let lh = sha256_hex(&l);
                let rh = sha256_hex(&r);
                if lh == rh {
                    self.record_with_base(provider, remote_key, &lh, local_path, &l)?;
                    self.db.clear_pending(remote_key)?;
                    return Ok(());
                }

                if last_hash == Some(rh.as_str()) {
                    provider.write_bytes(remote_key, &l)?;
                    self.record_with_base(provider, remote_key, &lh, local_path, &l)?;
                    report.pushed += 1;
                    activity(report, "push", remote_key, "Pushed updated file");
                } else if last_hash == Some(lh.as_str()) {
                    if gated
                        && !self.approval_gate(
                            remote_key,
                            &rh,
                            "Hook script changed on another PC",
                            &preview_text(&r),
                        )?
                    {
                        return Ok(());
                    }
                    if !write_local_if_unchanged(local_path, &r, &local_fp)? {
                        report.deferred += 1;
                        return Ok(());
                    }
                    self.record_with_base(provider, remote_key, &rh, local_path, &r)?;
                    report.pulled += 1;
                    activity(report, "pull", remote_key, "Pulled updated file");
                } else {
                    let ctx = GenericConflict {
                        local_path,
                        remote_key,
                        local: &l,
                        remote: &r,
                        local_hash: &lh,
                        remote_hash: &rh,
                        last_hash,
                        local_fp: &local_fp,
                        gated,
                    };
                    self.merge_generic(provider, ctx, report)?;
                }
            }
            (None, None) => {
                if state.is_some() {
                    self.db.delete_state(remote_key)?;
                }
                self.db.clear_conflict(remote_key)?;
            }
        }

        Ok(())
    }

    /// Both PCs changed a file. Text files are 3-way merged against the last agreed version when
    /// the edits don't overlap; otherwise both copies are left untouched for the merge view.
    /// Binary files fall back to keeping the newest copy with a backup of the other.
    fn merge_generic(
        &self,
        provider: &dyn StorageProvider,
        c: GenericConflict<'_>,
        report: &mut SyncReport,
    ) -> Result<()> {
        let (Ok(ls), Ok(rs)) = (std::str::from_utf8(c.local), std::str::from_utf8(c.remote)) else {
            return self.newest_wins(provider, c, report);
        };
        let base = c.last_hash.and_then(|h| self.base_text(c.remote_key, h));
        // Hook scripts run code, so even clean merges are shown to the user first.
        if !c.gated {
            if let Some(merged) = base.as_deref().and_then(|b| auto_result(&merge3(b, ls, rs))) {
                if !write_local_if_unchanged(c.local_path, merged.as_bytes(), c.local_fp)? {
                    report.deferred += 1;
                    return Ok(());
                }
                provider.write_bytes(c.remote_key, merged.as_bytes())?;
                let h = sha256_hex(merged.as_bytes());
                self.record_with_base(provider, c.remote_key, &h, c.local_path, merged.as_bytes())?;
                report.merged += 1;
                activity(report, "merge", c.remote_key, "Merged non-overlapping edits from both PCs");
                return Ok(());
            }
        }
        self.hold_conflict(
            c.remote_key,
            c.local_path,
            c.local_hash,
            c.remote_hash,
            base.is_some(),
            report,
        )
    }

    fn newest_wins(
        &self,
        provider: &dyn StorageProvider,
        c: GenericConflict<'_>,
        report: &mut SyncReport,
    ) -> Result<()> {
        let local_mtime = fs::metadata(c.local_path).ok().and_then(|m| m.modified().ok());
        let remote_mtime: Option<SystemTime> = provider.modified_time(c.remote_key);

        let backup = if local_mtime >= remote_mtime {
            let backup = self.backup_conflict(c.remote_key, c.remote)?;
            provider.write_bytes(c.remote_key, c.local)?;
            self.record_with_base(provider, c.remote_key, c.local_hash, c.local_path, c.local)?;
            report.pushed += 1;
            backup
        } else {
            if c.gated
                && !self.approval_gate(
                    c.remote_key,
                    c.remote_hash,
                    "Hook file changed on another PC (conflicts with local edits)",
                    &preview_text(c.remote),
                )?
            {
                return Ok(());
            }
            let backup = self.backup_conflict(c.remote_key, c.local)?;
            if !write_local_if_unchanged(c.local_path, c.remote, c.local_fp)? {
                report.deferred += 1;
                return Ok(());
            }
            self.record_with_base(provider, c.remote_key, c.remote_hash, c.local_path, c.remote)?;
            report.pulled += 1;
            backup
        };
        report.conflicts.push(c.remote_key.to_string());
        activity(
            report,
            "conflict",
            c.remote_key,
            format!(
                "Binary file changed on both PCs (newest kept; backup at {})",
                backup.display()
            ),
        );
        Ok(())
    }

    /// Leaves both copies untouched and records the conflict for the merge view.
    fn hold_conflict(
        &self,
        key: &str,
        local_path: &Path,
        local_hash: &str,
        remote_hash: &str,
        has_base: bool,
        report: &mut SyncReport,
    ) -> Result<()> {
        let is_new = self.db.upsert_conflict(&MergeConflict {
            remote_key: key.to_string(),
            local_path: path_str(local_path),
            local_hash: local_hash.to_string(),
            remote_hash: remote_hash.to_string(),
            has_base,
            detected_at: Utc::now(),
        })?;
        report.conflicts.push(key.to_string());
        if is_new {
            let detail = if has_base {
                "Both PCs changed the same part of this file; both copies are kept unchanged until you merge them (Review tab)"
            } else {
                "This file differs between the PCs and no common version is known yet; both copies are kept unchanged until you merge them (Review tab)"
            };
            activity(report, "conflict", key, detail);
        }
        Ok(())
    }

    fn base_text(&self, key: &str, hash: &str) -> Option<String> {
        self.db
            .get_base(key, hash)
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok())
    }

    /// Records the agreed state, remembers the content as the next merge ancestor,
    /// and clears any held conflict for the file.
    fn record_with_base(
        &self,
        provider: &dyn StorageProvider,
        key: &str,
        hash: &str,
        local_path: &Path,
        content: &[u8],
    ) -> Result<()> {
        self.record(provider, key, hash, local_path)?;
        if content.len() <= BASE_LIMIT {
            self.db.set_base(key, hash, content)?;
        } else {
            self.db.delete_base(key)?;
        }
        self.db.clear_conflict(key)
    }

    /// Files held because both PCs changed them.
    pub fn conflicts(&self) -> Result<Vec<MergeConflict>> {
        self.db.conflicts()
    }

    fn conflict_sides(&self, key: &str) -> Result<ConflictSides> {
        let conflict = self.db.get_conflict(key)?.ok_or_else(|| {
            SessionShareError::Sync(format!("{} has no unresolved conflict.", key))
        })?;
        let provider = self.provider()?;
        let local_path = PathBuf::from(&conflict.local_path);
        let missing = || {
            SessionShareError::Sync(format!(
                "{} no longer exists on both PCs; run a sync to refresh.",
                key
            ))
        };
        let local_bytes = if local_path.is_file() {
            read_with_retry(&local_path)?
        } else {
            return Err(missing());
        };
        let remote_bytes = provider.read_bytes(key)?.ok_or_else(missing)?;
        let state = self.db.get_state(key)?;
        let base_raw =
            tracked_hash(&state, &path_str(&local_path)).and_then(|h| self.base_text(key, h));

        if key == SETTINGS_KEY {
            let ctx = self.rewrite_ctx(None);
            let ex = &self.config.settings_excluded_keys;
            let local_raw = lossy(local_bytes);
            let remote_raw = lossy(remote_bytes);
            let local_portable = strip_keys(&normalize_settings_json(&local_raw, &ctx), ex);
            let view = |portable: &str| strip_keys(&denormalize_settings_json(portable, &ctx), ex);
            Ok(ConflictSides {
                kind: "json",
                local_hash: sha256_hex(local_portable.as_bytes()),
                remote_hash: sha256_hex(remote_raw.as_bytes()),
                local_text: view(&local_portable),
                remote_text: view(&remote_raw),
                base_text: base_raw.as_deref().map(view),
                local_raw: Some(local_raw),
                local_path,
            })
        } else {
            Ok(ConflictSides {
                kind: "text",
                local_hash: sha256_hex(&local_bytes),
                remote_hash: sha256_hex(&remote_bytes),
                local_text: lossy(local_bytes),
                remote_text: lossy(remote_bytes),
                base_text: base_raw,
                local_raw: None,
                local_path,
            })
        }
    }

    /// Side-by-side data for one held conflict, computed from the current files.
    pub fn conflict_view(&self, key: &str) -> Result<ConflictView> {
        let s = self.conflict_sides(key)?;
        let chunks = match &s.base_text {
            Some(b) => merge3(b, &s.local_text, &s.remote_text),
            None => merge2(&s.local_text, &s.remote_text),
        };
        Ok(ConflictView {
            remote_key: key.to_string(),
            local_path: path_str(&s.local_path),
            kind: s.kind.to_string(),
            has_base: s.base_text.is_some(),
            local_hash: s.local_hash,
            remote_hash: s.remote_hash,
            local_text: s.local_text,
            remote_text: s.remote_text,
            chunks,
        })
    }

    /// Writes the user's merged `content` to both PCs. The hashes must match what the merge view
    /// was built from, so edits made on either PC in the meantime are never overwritten.
    pub fn resolve_conflict(
        &self,
        key: &str,
        local_hash: &str,
        remote_hash: &str,
        content: &str,
    ) -> Result<()> {
        let s = self.conflict_sides(key)?;
        if s.local_hash != local_hash || s.remote_hash != remote_hash {
            return Err(SessionShareError::Sync(format!(
                "{} changed on one of the PCs after the merge view was opened. Reopen it to see the latest versions.",
                key
            )));
        }
        let provider = self.provider()?;
        let (local_bytes, remote_bytes) = if s.kind == "json" {
            serde_json::from_str::<serde_json::Value>(content).map_err(|e| {
                SessionShareError::Sync(format!("The merged settings.json is not valid JSON: {}", e))
            })?;
            let ctx = self.rewrite_ctx(None);
            let ex = &self.config.settings_excluded_keys;
            let local = reinject_keys(content, s.local_raw.as_deref(), ex);
            let portable = strip_keys(&normalize_settings_json(content, &ctx), ex);
            (local.into_bytes(), portable.into_bytes())
        } else {
            (content.as_bytes().to_vec(), content.as_bytes().to_vec())
        };

        atomic_write(&s.local_path, &local_bytes)?;
        provider.write_bytes(key, &remote_bytes)?;
        self.record_with_base(
            &provider,
            key,
            &sha256_hex(&remote_bytes),
            &s.local_path,
            &remote_bytes,
        )?;
        // Reviewing the merged content counts as approving any hook changes it contains.
        self.db.clear_pending(key)?;
        self.db.record_activity(&ActivityEntry {
            timestamp: Utc::now(),
            kind: "merge".to_string(),
            path: key.to_string(),
            detail: "Merged manually; both PCs now have the merged version".to_string(),
        })?;
        Ok(())
    }

    /// Purges expired trash / conflict backups at most once a day.
    fn maybe_cleanup(&mut self, provider: &LocalFolderProvider) {
        if self
            .last_cleanup
            .map(|t| t.elapsed() < CLEANUP_INTERVAL)
            .unwrap_or(false)
        {
            return;
        }
        let days = self.config.retention_days.max(1);
        purge_old_batches(&self.trash_dir(), days);
        purge_old_batches(&self.conflicts_dir(), days);
        purge_old_batches(&provider.root().join(REMOTE_TRASH_DIR), days);
        self.purge_legacy_conflict_files(days);
        self.last_cleanup = Some(Instant::now());
    }

    /// Older versions wrote `<file>.conflict.<timestamp>` backups next to the original file.
    fn purge_legacy_conflict_files(&self, days: u64) {
        let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(days * 24 * 3600))
        else {
            return;
        };
        let claude = &self.config.claude_dir;
        let mut roots: Vec<(PathBuf, usize)> = vec![(claude.clone(), 1), (claude.join("projects"), 3)];
        for d in ["skills", "agents", "commands", "plans", "rules", "hooks"] {
            roots.push((claude.join(d), usize::MAX));
        }
        for (root, depth) in roots {
            for entry in WalkDir::new(&root)
                .max_depth(depth)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !entry.file_type().is_file()
                    || !entry.file_name().to_string_lossy().contains(".conflict.")
                {
                    continue;
                }
                let old = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| t < cutoff)
                    .unwrap_or(false);
                if old {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::GlobalSyncOptions;
    use tempfile::TempDir;

    #[test]
    fn progress_reports_checked_files_and_actual_changes() {
        let tmp = TempDir::new().unwrap();
        let claude = tmp.path().join("claude");
        fs::create_dir_all(claude.join("skills")).unwrap();
        fs::write(claude.join("skills").join("alpha.md"), "alpha").unwrap();
        fs::write(claude.join("skills").join("beta.md"), "beta").unwrap();
        fs::create_dir_all(claude.join("agents")).unwrap();
        fs::write(claude.join("agents").join("disabled.md"), "ignored").unwrap();

        let mut cfg = AppConfig::new_default(claude);
        cfg.sync_dir = Some(tmp.path().join("sync"));
        cfg.global_options = GlobalSyncOptions {
            skills: true,
            agents: false,
            commands: false,
            plans: false,
            rules: false,
            hooks: false,
            claude_md: false,
            settings_json: false,
            history_jsonl: false,
            project_memory: false,
        };
        let mut engine = SyncEngine::new(tmp.path().join("app"), cfg).unwrap();
        let mut updates = Vec::new();
        let report = engine.sync_all_with_progress(|p| updates.push(p.clone())).unwrap();

        let first = updates.iter().find(|p| p.current_item.as_deref() == Some("global/skills/alpha.md")).unwrap();
        assert_eq!((first.checked, first.completed, first.total), (0, 0, 2));
        assert_eq!(first.upcoming, vec!["global/skills/beta.md"]);
        let second = updates.iter().find(|p| p.current_item.as_deref() == Some("global/skills/beta.md")).unwrap();
        assert_eq!((second.checked, second.completed, second.total), (1, 1, 2));
        assert_eq!(second.recent[0].path, "global/skills/alpha.md");
        assert_eq!(second.recent[0].kind, "push");
        let last = updates.last().unwrap();
        assert_eq!(last.phase, "Finishing sync");
        assert_eq!(last.checked, 2);
        assert_eq!(report.pushed, 2);
        assert!(!updates.iter().any(|p| p.current_item.as_deref() == Some("global/agents/disabled.md")));

        updates.clear();
        let report = engine.sync_all_with_progress(|p| updates.push(p.clone())).unwrap();
        assert_eq!(report.pushed, 0);
        assert_eq!(updates.last().unwrap().checked, 2);
        assert!(updates.last().unwrap().recent.is_empty());
    }

    #[test]
    fn auto_resolves_different_usernames_and_custom_claude_dirs_without_manual_mapping() {
        let tmp = TempDir::new().unwrap();
        let sync_drive = tmp.path().join("OneDrive_SessionShare");

        // Machine A: Home = tmp/Users/PersonalUser, Claude = Home/.claude, Project = Home/Dev/Portal
        let home_a = tmp.path().join("Users").join("PersonalUser");
        let claude_a = home_a.join(".claude");
        let repo_a = home_a.join("Dev").join("Portal");
        fs::create_dir_all(&repo_a).unwrap();
        let repo_a_str = normalize_windows_path(&repo_a.to_string_lossy());
        let proj_dir_a = claude_a.join("projects").join(encode_project_path(&repo_a_str));
        fs::create_dir_all(&proj_dir_a).unwrap();

        let session_a = format!(
            "{{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2026-01-01T10:00:00Z\",\"cwd\":{},\"toolUse\":{{\"file_path\":{}}},\"message\":{{\"content\":\"hello\"}}}}\n",
            serde_json::to_string(&repo_a_str).unwrap(),
            serde_json::to_string(&format!("{}\\src\\app.ts", repo_a_str)).unwrap()
        );
        fs::write(proj_dir_a.join("sess-100.jsonl"), session_a).unwrap();

        // Machine A also has settings.json referencing its own .claude/hooks and plans/
        let settings_a = format!(
            "{{\"hooks\":{{\"PreToolUse\":[{{\"command\":{}}}]}}}}",
            serde_json::to_string(&format!(
                "pwsh -File {}\\hooks\\check.ps1",
                normalize_windows_path(&claude_a.to_string_lossy())
            ))
            .unwrap()
        );
        fs::write(claude_a.join("settings.json"), settings_a).unwrap();
        fs::create_dir_all(claude_a.join("plans")).unwrap();
        fs::write(claude_a.join("plans").join("p1.md"), "# Plan 1").unwrap();

        let mut cfg_a = AppConfig::new_default(claude_a.clone());
        cfg_a.machine_name = "Personal-PC".to_string();
        cfg_a.sync_dir = Some(sync_drive.clone());
        let mut engine_a = SyncEngine::new(tmp.path().join("appdata_a"), cfg_a)
            .unwrap()
            .with_home_dir(home_a);
        let mut progress = Vec::new();
        let rep_a = engine_a
            .sync_all_with_progress(|update| progress.push(update.clone()))
            .unwrap();
        assert!(rep_a.pushed >= 3);
        assert!(progress.iter().any(|update| {
            update.phase.contains("sessions")
                && update.current_item.as_deref().is_some_and(|item| item.ends_with("/sess-100.jsonl"))
        }));

        // Machine B: Different Windows username AND custom Claude config dir, AND Claude has never run yet!
        let home_b = tmp.path().join("Users").join("OfficeUser");
        let claude_b = tmp.path().join("CustomOfficeClaude");
        let repo_b = home_b.join("Dev").join("Portal");
        fs::create_dir_all(&repo_b).unwrap();
        let repo_b_str = normalize_windows_path(&repo_b.to_string_lossy());

        let mut cfg_b = AppConfig::new_default(claude_b.clone());
        cfg_b.machine_name = "Office-Laptop".to_string();
        cfg_b.sync_dir = Some(sync_drive.clone());
        cfg_b.require_hook_approval = false;
        let mut engine_b = SyncEngine::new(tmp.path().join("appdata_b"), cfg_b)
            .unwrap()
            .with_home_dir(home_b);

        // Zero manual mapping! sync_all() should auto-resolve Portal via home-relative hint!
        let rep_b = engine_b.sync_all().unwrap();
        assert_eq!(rep_b.auto_mapped, 1);
        assert!(rep_b.pulled >= 3);

        let proj_dir_b = claude_b.join("projects").join(encode_project_path(&repo_b_str));
        let pulled_jsonl = fs::read_to_string(proj_dir_b.join("sess-100.jsonl")).unwrap();
        assert!(pulled_jsonl.contains(&serde_json::to_string(&repo_b_str).unwrap()));
        assert!(pulled_jsonl.contains("OfficeUser"));
        assert!(!pulled_jsonl.contains("PersonalUser"));

        let pulled_settings = fs::read_to_string(claude_b.join("settings.json")).unwrap();
        assert!(pulled_settings.contains("CustomOfficeClaude"));
        assert!(!pulled_settings.contains("PersonalUser"));
        assert!(claude_b.join("plans").join("p1.md").is_file());
    }

    #[test]
    fn auto_resolves_completely_different_folder_names_via_workspace_roots_and_git_remote() {
        let tmp = TempDir::new().unwrap();
        let sync_drive = tmp.path().join("SyncDrive");

        // Machine A: repo at tmp/A_Dev/personal-app
        let claude_a = tmp.path().join("claude_a");
        let repo_a = tmp.path().join("A_Dev").join("personal-app");
        fs::create_dir_all(repo_a.join(".git")).unwrap();
        fs::write(
            repo_a.join(".git").join("config"),
            "[remote \"origin\"]\n\turl = https://github.com/team/shared-service.git\n",
        )
        .unwrap();
        let repo_a_str = normalize_windows_path(&repo_a.to_string_lossy());
        let proj_dir_a = claude_a.join("projects").join(encode_project_path(&repo_a_str));
        fs::create_dir_all(&proj_dir_a).unwrap();
        fs::write(
            proj_dir_a.join("s1.jsonl"),
            format!(
                "{{\"type\":\"user\",\"uuid\":\"1\",\"cwd\":{},\"message\":{{\"content\":\"hi\"}}}}\n",
                serde_json::to_string(&repo_a_str).unwrap()
            ),
        )
        .unwrap();

        let mut cfg_a = AppConfig::new_default(claude_a);
        cfg_a.sync_dir = Some(sync_drive.clone());
        let mut engine_a = SyncEngine::new(tmp.path().join("app_a"), cfg_a)
            .unwrap()
            .with_home_dir(tmp.path().join("home_a"));
        engine_a.sync_all().unwrap();

        // Machine B: repo has a totally different folder name under workspace root `tmp/OfficeRepos/corp-svc-renamed`
        let claude_b = tmp.path().join("claude_b");
        let office_root = tmp.path().join("OfficeRepos");
        let repo_b = office_root.join("corp-svc-renamed");
        fs::create_dir_all(repo_b.join(".git")).unwrap();
        fs::write(
            repo_b.join(".git").join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:team/shared-service.git\n",
        )
        .unwrap();
        let repo_b_str = normalize_windows_path(&repo_b.to_string_lossy());

        let mut cfg_b = AppConfig::new_default(claude_b.clone());
        cfg_b.sync_dir = Some(sync_drive);
        cfg_b.workspace_roots = vec![office_root.to_string_lossy().to_string()];
        let mut engine_b = SyncEngine::new(tmp.path().join("app_b"), cfg_b)
            .unwrap()
            .with_home_dir(tmp.path().join("home_b"));

        let rep_b = engine_b.sync_all().unwrap();
        assert_eq!(rep_b.auto_mapped, 1);
        assert_eq!(rep_b.pulled, 1);

        let proj_dir_b = claude_b.join("projects").join(encode_project_path(&repo_b_str));
        let pulled = fs::read_to_string(proj_dir_b.join("s1.jsonl")).unwrap();
        assert!(pulled.contains("corp-svc-renamed"));
        assert!(!pulled.contains("personal-app"));
    }

    /// Two simulated PCs sharing one synced folder, with every optional asset disabled except the
    /// ones a test opts into.
    struct TwoPcs {
        tmp: TempDir,
        sync: PathBuf,
    }

    impl TwoPcs {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let sync = tmp.path().join("sync");
            Self { tmp, sync }
        }

        fn claude(&self, name: &str) -> PathBuf {
            self.tmp.path().join(name).join(".claude")
        }

        fn engine(&self, name: &str, tweak: impl FnOnce(&mut AppConfig)) -> SyncEngine {
            let mut cfg = AppConfig::new_default(self.claude(name));
            cfg.machine_name = name.to_string();
            cfg.sync_dir = Some(self.sync.clone());
            cfg.global_options.history_jsonl = false;
            tweak(&mut cfg);
            SyncEngine::new(self.tmp.path().join(name).join("appdata"), cfg)
                .unwrap()
                .with_home_dir(self.tmp.path().join(name))
        }
    }

    fn write_project_session(claude: &Path, repo: &Path, file: &str, lines: &[&str]) -> PathBuf {
        let repo_s = normalize_windows_path(&repo.to_string_lossy());
        fs::create_dir_all(repo).unwrap();
        let dir = claude.join("projects").join(encode_project_path(&repo_s));
        fs::create_dir_all(&dir).unwrap();
        let mut content = String::new();
        for uuid in lines {
            content.push_str(&format!(
                "{{\"type\":\"user\",\"uuid\":\"{}\",\"timestamp\":\"2026-01-01T00:00:0{}Z\",\"cwd\":{},\"message\":{{\"content\":\"m{}\"}}}}\n",
                uuid,
                uuid.len() % 10,
                serde_json::to_string(&repo_s).unwrap(),
                uuid
            ));
        }
        let path = dir.join(file);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn active_session_is_not_rewritten_until_idle() {
        let pcs = TwoPcs::new();
        let repo = pcs.tmp.path().join("a").join("Dev").join("App");
        let session = write_project_session(&pcs.claude("a"), &repo, "s1.jsonl", &["u1"]);
        let mut a = pcs.engine("a", |_| {});
        a.sync_all().unwrap();

        // Another PC appended a line to the synced copy.
        let remote = fs::read_dir(pcs.sync.join("projects"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path()
            .join("sessions")
            .join("s1.jsonl");
        let mut content = fs::read_to_string(&remote).unwrap();
        content.push_str("{\"type\":\"user\",\"uuid\":\"u2\",\"timestamp\":\"2026-01-01T00:00:09Z\",\"message\":{\"content\":\"from-b\"}}\n");
        fs::write(&remote, content).unwrap();

        // Local transcript was just written (session active): the pull is deferred.
        let before = fs::read_to_string(&session).unwrap();
        let rep = a.sync_all().unwrap();
        assert_eq!(rep.deferred, 1);
        assert_eq!(rep.pulled, 0);
        assert_eq!(fs::read_to_string(&session).unwrap(), before);

        // Once idle it is applied.
        let mut cfg = a.config().clone();
        cfg.active_session_grace_secs = 0;
        a.update_config(cfg).unwrap();
        let rep = a.sync_all().unwrap();
        assert_eq!(rep.pulled, 1);
        assert!(fs::read_to_string(&session).unwrap().contains("from-b"));
    }

    #[test]
    fn deletions_propagate_to_trash_on_both_sides() {
        let pcs = TwoPcs::new();
        let skill_a = pcs.claude("a").join("skills").join("demo").join("SKILL.md");
        fs::create_dir_all(skill_a.parent().unwrap()).unwrap();
        fs::write(&skill_a, "# demo").unwrap();

        let mut a = pcs.engine("a", |_| {});
        let mut b = pcs.engine("b", |_| {});
        a.sync_all().unwrap();
        b.sync_all().unwrap();
        let skill_b = pcs.claude("b").join("skills").join("demo").join("SKILL.md");
        assert!(skill_b.is_file());

        fs::remove_file(&skill_b).unwrap();
        let rep_b = b.sync_all().unwrap();
        assert_eq!(rep_b.deleted, 1);
        assert!(!pcs.sync.join("global").join("skills").join("demo").join("SKILL.md").exists());
        assert!(fs::read_dir(pcs.sync.join(REMOTE_TRASH_DIR)).unwrap().next().is_some());

        let rep_a = a.sync_all().unwrap();
        assert_eq!(rep_a.deleted, 1);
        assert!(!skill_a.exists());
        assert!(pcs.tmp.path().join("a").join("appdata").join("trash").is_dir());

        // Nothing is resurrected afterwards.
        assert_eq!(b.sync_all().unwrap().pulled, 0);
        assert!(!skill_b.exists());
    }

    #[test]
    fn mass_deletion_is_not_propagated() {
        let pcs = TwoPcs::new();
        let cmds = pcs.claude("a").join("commands");
        fs::create_dir_all(&cmds).unwrap();
        for i in 0..8 {
            fs::write(cmds.join(format!("c{}.md", i)), format!("cmd {}", i)).unwrap();
        }
        let mut a = pcs.engine("a", |_| {});
        a.sync_all().unwrap();

        fs::remove_dir_all(&cmds).unwrap();
        let rep = a.sync_all().unwrap();
        assert_eq!(rep.deleted, 0);
        assert!(rep.errors.iter().any(|e| e.contains("safety guard")));
        assert!(cmds.join("c0.md").is_file());
        assert!(pcs.sync.join("global").join("commands").join("c0.md").is_file());
    }

    #[test]
    fn incoming_hooks_require_approval() {
        let pcs = TwoPcs::new();
        let claude_a = pcs.claude("a");
        fs::create_dir_all(claude_a.join("hooks")).unwrap();
        fs::write(claude_a.join("hooks").join("check.ps1"), "Write-Host hi").unwrap();
        fs::write(
            claude_a.join("settings.json"),
            r#"{"model":"opus","hooks":{"Stop":[{"command":"pwsh check.ps1"}]}}"#,
        )
        .unwrap();
        pcs.engine("a", |_| {}).sync_all().unwrap();

        let mut b = pcs.engine("b", |_| {});
        let rep = b.sync_all().unwrap();
        assert_eq!(rep.pending_approvals, 2);
        let claude_b = pcs.claude("b");
        assert!(!claude_b.join("hooks").join("check.ps1").exists());
        assert!(!claude_b.join("settings.json").exists());

        for p in b.pending_approvals().unwrap() {
            assert!(!p.preview.is_empty());
            b.resolve_approval(&p.remote_key, &p.remote_hash, true).unwrap();
        }
        let rep = b.sync_all().unwrap();
        assert_eq!(rep.pending_approvals, 0);
        assert!(claude_b.join("hooks").join("check.ps1").is_file());
        assert!(fs::read_to_string(claude_b.join("settings.json"))
            .unwrap()
            .contains("pwsh check.ps1"));
    }

    #[test]
    fn settings_env_never_leaves_the_pc() {
        let pcs = TwoPcs::new();
        fs::create_dir_all(pcs.claude("a")).unwrap();
        fs::write(
            pcs.claude("a").join("settings.json"),
            r#"{"model":"opus","env":{"API_KEY":"secret-a"}}"#,
        )
        .unwrap();
        fs::create_dir_all(pcs.claude("b")).unwrap();
        fs::write(
            pcs.claude("b").join("settings.json"),
            r#"{"model":"sonnet","env":{"API_KEY":"secret-b"}}"#,
        )
        .unwrap();

        pcs.engine("a", |_| {}).sync_all().unwrap();
        let remote = fs::read_to_string(pcs.sync.join("global").join("settings.json")).unwrap();
        assert!(!remote.contains("secret-a"));

        // B has never synced, so the differing settings are held for a merge; pick A's version.
        let mut b = pcs.engine("b", |_| {});
        let rep = b.sync_all().unwrap();
        assert_eq!(rep.unresolved_conflicts, 1);
        let view = b.conflict_view(SETTINGS_KEY).unwrap();
        assert!(!view.has_base);
        assert!(!view.local_text.contains("secret") && !view.remote_text.contains("secret"));
        b.resolve_conflict(SETTINGS_KEY, &view.local_hash, &view.remote_hash, &view.remote_text)
            .unwrap();

        let local_b = fs::read_to_string(pcs.claude("b").join("settings.json")).unwrap();
        assert!(local_b.contains("opus"));
        assert!(local_b.contains("secret-b"));
        assert!(!local_b.contains("secret-a"));
        let remote = fs::read_to_string(pcs.sync.join("global").join("settings.json")).unwrap();
        assert!(!remote.contains("secret"));
        assert_eq!(b.sync_all().unwrap().unresolved_conflicts, 0);
    }

    /// Both PCs start from a shared, already-synced CLAUDE.md.
    fn shared_claude_md(pcs: &TwoPcs, content: &str) -> (SyncEngine, SyncEngine) {
        fs::create_dir_all(pcs.claude("a")).unwrap();
        fs::write(pcs.claude("a").join("CLAUDE.md"), content).unwrap();
        let mut a = pcs.engine("a", |_| {});
        let mut b = pcs.engine("b", |_| {});
        a.sync_all().unwrap();
        b.sync_all().unwrap();
        (a, b)
    }

    #[test]
    fn non_overlapping_edits_on_both_pcs_merge_automatically() {
        let pcs = TwoPcs::new();
        let (mut a, mut b) = shared_claude_md(&pcs, "line one\nline two\nline three\n");
        fs::write(pcs.claude("a").join("CLAUDE.md"), "line ONE from A\nline two\nline three\n").unwrap();
        a.sync_all().unwrap();
        fs::write(pcs.claude("b").join("CLAUDE.md"), "line one\nline two\nline three, edited on B\n").unwrap();

        let rep = b.sync_all().unwrap();
        assert_eq!((rep.merged, rep.unresolved_conflicts), (1, 0));
        let expected = "line ONE from A\nline two\nline three, edited on B\n";
        assert_eq!(fs::read_to_string(pcs.claude("b").join("CLAUDE.md")).unwrap(), expected);
        a.sync_all().unwrap();
        assert_eq!(fs::read_to_string(pcs.claude("a").join("CLAUDE.md")).unwrap(), expected);
    }

    #[test]
    fn overlapping_edits_are_held_until_merged_by_hand() {
        let pcs = TwoPcs::new();
        let (mut a, mut b) = shared_claude_md(&pcs, "intro\nshared rule\noutro\n");
        fs::write(pcs.claude("a").join("CLAUDE.md"), "intro\nrule from A\noutro\n").unwrap();
        a.sync_all().unwrap();
        let b_version = "intro\nrule from B (longer)\noutro\n";
        fs::write(pcs.claude("b").join("CLAUDE.md"), b_version).unwrap();

        let rep = b.sync_all().unwrap();
        assert_eq!(rep.unresolved_conflicts, 1);
        // Nothing was overwritten on either side.
        assert_eq!(fs::read_to_string(pcs.claude("b").join("CLAUDE.md")).unwrap(), b_version);
        assert!(fs::read_to_string(pcs.sync.join("global").join("CLAUDE.md"))
            .unwrap()
            .contains("rule from A"));
        // Re-syncing doesn't log the same conflict again.
        assert!(b.sync_all().unwrap().activities.is_empty());

        let view = b.conflict_view("global/CLAUDE.md").unwrap();
        assert!(view.has_base);
        let open: Vec<_> = view.chunks.iter().filter(|c| !c.stable).collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].base.as_deref(), Some(&["shared rule\n".to_string()][..]));

        // A changes the file again: saving a merge built from the old view is refused.
        fs::write(pcs.claude("a").join("CLAUDE.md"), "intro\nrule from A v2\noutro\n").unwrap();
        a.sync_all().unwrap();
        let merged = "intro\nrule from A\nrule from B (longer)\noutro\n";
        assert!(b
            .resolve_conflict("global/CLAUDE.md", &view.local_hash, &view.remote_hash, merged)
            .is_err());

        let view = b.conflict_view("global/CLAUDE.md").unwrap();
        b.resolve_conflict("global/CLAUDE.md", &view.local_hash, &view.remote_hash, merged)
            .unwrap();
        assert!(b.conflicts().unwrap().is_empty());
        a.sync_all().unwrap();
        assert_eq!(fs::read_to_string(pcs.claude("a").join("CLAUDE.md")).unwrap(), merged);
    }

    #[test]
    fn settings_changes_to_different_keys_merge_automatically() {
        let pcs = TwoPcs::new();
        fs::create_dir_all(pcs.claude("a")).unwrap();
        fs::write(pcs.claude("a").join("settings.json"), r#"{"model":"opus","theme":"dark"}"#).unwrap();
        let mut a = pcs.engine("a", |_| {});
        let mut b = pcs.engine("b", |_| {});
        a.sync_all().unwrap();
        b.sync_all().unwrap();

        fs::write(pcs.claude("a").join("settings.json"), r#"{"model":"sonnet","theme":"dark"}"#).unwrap();
        a.sync_all().unwrap();
        fs::write(
            pcs.claude("b").join("settings.json"),
            r#"{"model":"opus","theme":"light","verbose":true}"#,
        )
        .unwrap();
        let rep = b.sync_all().unwrap();
        assert_eq!((rep.merged, rep.unresolved_conflicts), (1, 0));
        let merged: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pcs.claude("b").join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(merged["model"], "sonnet");
        assert_eq!(merged["theme"], "light");
        assert_eq!(merged["verbose"], true);
    }

    #[test]
    fn idle_sync_writes_nothing_to_the_synced_drive() {
        let pcs = TwoPcs::new();
        let repo = pcs.tmp.path().join("a").join("Dev").join("App");
        write_project_session(&pcs.claude("a"), &repo, "s1.jsonl", &["u1", "u22"]);
        fs::write(pcs.claude("a").join("CLAUDE.md"), "# global").unwrap();
        let mut a = pcs.engine("a", |_| {});
        a.sync_all().unwrap();

        let snapshot = || -> Vec<(PathBuf, Option<String>)> {
            WalkDir::new(&pcs.sync)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| (e.path().to_path_buf(), local_fingerprint(e.path())))
                .collect()
        };
        let before = snapshot();
        let rep = a.sync_all().unwrap();
        assert_eq!(rep.pushed + rep.pulled + rep.merged, 0);
        assert_eq!(snapshot(), before);
    }

    #[test]
    fn listing_projects_does_not_write() {
        let pcs = TwoPcs::new();
        let repo = pcs.tmp.path().join("a").join("Dev").join("App");
        write_project_session(&pcs.claude("a"), &repo, "s1.jsonl", &["u1"]);
        let mut a = pcs.engine("a", |_| {});
        let (projects, sessions) = a.overview().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(sessions.len(), 1);
        assert!(!pcs.sync.join(REGISTRY_PATH).exists());
        assert!(!pcs.sync.join("machines").exists());
    }
}

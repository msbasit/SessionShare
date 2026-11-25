use crate::claude_fs::{
    auto_resolve_unmapped_project, canonical_id_from_git, canonical_id_from_name,
    compute_home_relative_hint, default_home_dir, display_name_from_path, encode_project_path,
    inspect_session_file, normalize_windows_path, resolve_encoded_project_dir, scan_local_projects,
};
use crate::error::{Result, SessionShareError};
use crate::merger::{merge_jsonl, MergeOutcome};
use crate::models::{
    ActivityEntry, AppConfig, CanonicalProject, MachineManifest, ProjectView, Registry,
    SessionSummary, SyncReport,
};
use crate::path_rewrite::{
    denormalize_history_jsonl, denormalize_jsonl_with_ctx, denormalize_settings_json,
    normalize_history_jsonl, normalize_jsonl_with_ctx, normalize_settings_json, PathRewriteContext,
};
use crate::state_db::{save_config, StateDb};
use crate::storage::{
    atomic_write, read_with_retry, sha256_hex, LocalFolderProvider, StorageProvider, TMP_SUFFIX,
};
use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const REGISTRY_PATH: &str = "registry.json";

pub struct SyncEngine {
    app_data_dir: PathBuf,
    config: AppConfig,
    db: StateDb,
    /// Optional home dir override (primarily for deterministic cross-machine tests)
    home_dir_override: Option<String>,
}

impl SyncEngine {
    pub fn new(app_data_dir: PathBuf, config: AppConfig) -> Result<Self> {
        let db = StateDb::open(&app_data_dir)?;
        Ok(Self {
            app_data_dir,
            config,
            db,
            home_dir_override: None,
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
        self.config = config;
        Ok(())
    }

    pub fn recent_activities(&self, limit: usize) -> Result<Vec<ActivityEntry>> {
        self.db.recent_activities(limit)
    }

    fn provider(&self) -> Result<LocalFolderProvider> {
        let sync_dir = self.config.sync_dir.clone().ok_or_else(|| {
            SessionShareError::Config("Synced drive folder is not configured yet.".to_string())
        })?;
        LocalFolderProvider::new(sync_dir)
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

    fn save_own_manifest(&self, provider: &dyn StorageProvider) -> Result<()> {
        let manifest = MachineManifest {
            machine_id: self.config.machine_id.clone(),
            machine_name: self.config.machine_name.clone(),
            updated_at: Utc::now(),
            project_paths: self.config.local_project_paths.clone(),
        };
        let path = format!("machines/{}.json", self.config.machine_id);
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        provider.write_bytes(&path, &bytes)
    }

    /// Builds the list of workspace roots to search when auto-resolving unmapped remote projects:
    /// combines explicitly configured `workspace_roots` with parent directories of already-known projects.
    fn collect_search_roots(&self) -> Vec<PathBuf> {
        let mut seen = BTreeSet::new();
        let mut roots = Vec::new();

        for r in &self.config.workspace_roots {
            let norm = normalize_windows_path(r);
            if !norm.is_empty() && seen.insert(norm.to_ascii_lowercase()) {
                roots.push(PathBuf::from(norm));
            }
        }

        // Infer sibling workspace roots from already-mapped local projects
        for local_path in self.config.local_project_paths.values() {
            if let Some(parent) = Path::new(local_path).parent() {
                let p_str = normalize_windows_path(&parent.to_string_lossy());
                if p_str.len() > 3 && seen.insert(p_str.to_ascii_lowercase()) {
                    roots.push(PathBuf::from(p_str));
                }
            }
        }

        roots
    }

    /// Discovers local Claude projects, reconciles them with `registry.json`, and runs smart
    /// disk auto-discovery for any unmapped remote projects whose folder layout differs on this PC.
    pub fn discover_projects(&mut self) -> Result<Vec<ProjectView>> {
        let (views, _) = self.discover_projects_internal()?;
        Ok(views)
    }

    fn discover_projects_internal(&mut self) -> Result<(Vec<ProjectView>, usize)> {
        let provider = self.provider()?;
        let mut registry = self.load_registry(&provider)?;
        let local_projects = scan_local_projects(&self.config.claude_dir)?;
        let home_dir = self.effective_home_dir();
        let mut registry_dirty = false;
        let mut config_dirty = false;
        let mut auto_mapped_count = 0usize;

        // Index existing local_project_paths by normalized path -> canonical_id
        let mut local_path_to_id: BTreeMap<String, String> = BTreeMap::new();
        for (cid, path) in &self.config.local_project_paths {
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
                    self.config
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

            if self.config.local_project_paths.get(&canonical_id) != Some(&local_path) {
                self.config
                    .local_project_paths
                    .insert(canonical_id.clone(), local_path.clone());
                self.config
                    .mapping_sources
                    .insert(canonical_id.clone(), source);
                local_path_to_id.insert(norm_key, canonical_id.clone());
                config_dirty = true;
            }

            if !self.config.project_sync_enabled.contains_key(&canonical_id) {
                self.config
                    .project_sync_enabled
                    .insert(canonical_id.clone(), true);
                config_dirty = true;
            }

            local_counts.insert(canonical_id, lp.session_files.len());
        }

        // Smart Auto-Resolver: for any remote project not yet mapped on this PC,
        // automatically search %USERPROFILE%-relative paths and workspace roots!
        let search_roots = self.collect_search_roots();
        for (cid, proj) in &registry.projects {
            if self.config.local_project_paths.contains_key(cid) {
                continue;
            }
            if let Some((resolved_path, source)) =
                auto_resolve_unmapped_project(proj, home_dir.as_deref(), &search_roots)
            {
                let norm_key = resolved_path.to_ascii_lowercase();
                // Avoid colliding with another already-mapped project at the exact same path
                if !local_path_to_id.contains_key(&norm_key) {
                    self.config
                        .local_project_paths
                        .insert(cid.clone(), resolved_path.clone());
                    self.config
                        .mapping_sources
                        .insert(cid.clone(), source.to_string());
                    self.config
                        .project_sync_enabled
                        .entry(cid.clone())
                        .or_insert(true);
                    local_path_to_id.insert(norm_key, cid.clone());
                    config_dirty = true;
                    auto_mapped_count += 1;
                }
            }
        }

        if registry_dirty {
            self.save_registry(&provider, &registry)?;
        }
        if config_dirty {
            save_config(&self.app_data_dir, &self.config)?;
        }
        self.save_own_manifest(&provider)?;

        let manifests = self.load_machine_manifests(&provider)?;

        let mut views = Vec::new();
        for (cid, proj) in &registry.projects {
            let local_path = self.config.local_project_paths.get(cid).cloned();
            let encoded_local_dir = local_path.as_deref().map(|p| {
                resolve_encoded_project_dir(&self.config.claude_dir, p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| encode_project_path(p))
            });
            let mapping_source = self.config.mapping_sources.get(cid).cloned();

            let remote_machines: Vec<String> = manifests
                .iter()
                .filter(|m| {
                    m.machine_id != self.config.machine_id && m.project_paths.contains_key(cid)
                })
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

            let sync_enabled = self
                .config
                .project_sync_enabled
                .get(cid)
                .copied()
                .unwrap_or(false);

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
                local_session_count: local_counts.get(cid).copied().unwrap_or(0),
                remote_session_count,
            });
        }

        views.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        });
        Ok((views, auto_mapped_count))
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

    /// Lists all sessions across local and remote synced projects.
    pub fn list_sessions(&mut self) -> Result<Vec<SessionSummary>> {
        let projects = self.discover_projects()?;
        let provider = self.provider()?;
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

            if let Some(dir) = &local_dir {
                if dir.is_dir() {
                    if let Ok(rd) = fs::read_dir(dir) {
                        for entry in rd.flatten() {
                            let p = entry.path();
                            if p.is_file()
                                && p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                            {
                                if let Some(sid) = p.file_stem().and_then(|s| s.to_str()) {
                                    let sid_str = sid.to_string();
                                    let meta = inspect_session_file(&p);
                                    seen_ids.insert(sid_str.clone());
                                    sessions.push(SessionSummary {
                                        in_remote: remote_files.contains(&sid_str),
                                        in_local: true,
                                        session_id: sid_str,
                                        project_id: proj.canonical_id.clone(),
                                        project_name: proj.display_name.clone(),
                                        local_project_path: proj.local_path.clone(),
                                        title: meta.title,
                                        last_modified: meta.last_modified,
                                        line_count: meta.line_count,
                                        size_bytes: meta.size_bytes,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Remote-only sessions (not yet pulled or project not yet mapped)
            for sid in remote_files {
                if seen_ids.contains(&sid) {
                    continue;
                }
                let remote_file = provider
                    .root()
                    .join(&remote_prefix)
                    .join(format!("{}.jsonl", sid));
                let meta = inspect_session_file(&remote_file);
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
        let mut report = SyncReport {
            timestamp: Utc::now(),
            ..Default::default()
        };

        // Ensure base ~/.claude directory exists even on a fresh PC
        let _ = fs::create_dir_all(&self.config.claude_dir);

        let (projects, auto_mapped) = self.discover_projects_internal()?;
        report.auto_mapped = auto_mapped;
        if auto_mapped > 0 {
            report.activities.push(ActivityEntry {
                timestamp: Utc::now(),
                kind: "auto-map".to_string(),
                path: "registry.json".to_string(),
                detail: format!(
                    "Automatically resolved {} project folder(s) on this PC",
                    auto_mapped
                ),
            });
        }

        let provider = self.provider()?;

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
                &mut report,
            ) {
                report
                    .errors
                    .push(format!("Project {} sessions: {}", proj.display_name, e));
            }

            if self.config.global_options.project_memory {
                let local_mem = local_claude_proj_dir.join("memory");
                let remote_mem = format!("projects/{}/memory", proj.canonical_id);
                if let Err(e) =
                    self.sync_generic_dir(&provider, &local_mem, &remote_mem, &mut report)
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
                ) {
                    report.errors.push(format!("{}: {}", dir_name, e));
                }
            }
        }

        // 3. Sync global single files
        if opts.claude_md {
            if let Err(e) = self.sync_generic_file(
                &provider,
                &self.config.claude_dir.join("CLAUDE.md"),
                "global/CLAUDE.md",
                &mut report,
            ) {
                report.errors.push(format!("CLAUDE.md: {}", e));
            }
        }
        if opts.settings_json {
            if let Err(e) = self.sync_settings_json(&provider, &mut report) {
                report.errors.push(format!("settings.json: {}", e));
            }
        }
        if opts.history_jsonl {
            if let Err(e) = self.sync_history_jsonl(&provider, &mut report) {
                report.errors.push(format!("history.jsonl: {}", e));
            }
        }

        // Persist activities
        for act in &report.activities {
            let _ = self.db.record_activity(act);
        }

        Ok(report)
    }

    fn sync_project_sessions(
        &self,
        provider: &dyn StorageProvider,
        canonical_id: &str,
        local_root: &str,
        local_claude_proj_dir: &Path,
        report: &mut SyncReport,
    ) -> Result<()> {
        let remote_prefix = format!("projects/{}/sessions", canonical_id);
        let ctx = self.rewrite_ctx(Some(local_root));

        // Gather all local .jsonl files recursively (main transcripts + subagent transcripts)
        let mut rel_files: BTreeSet<String> = BTreeSet::new();
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
                        rel_files.insert(rel_posix);
                    }
                }
            }
        }

        // Gather all remote .jsonl files
        for remote_full in provider.list_files(&remote_prefix)? {
            if let Some(rel) = remote_full.strip_prefix(&format!("{}/", remote_prefix)) {
                if rel.ends_with(".jsonl") && !rel.contains(".conflict.") {
                    rel_files.insert(rel.to_string());
                }
            }
        }

        let mut pulled_any = false;

        for rel in rel_files {
            let local_path = local_claude_proj_dir.join(rel.replace('/', "\\"));
            let remote_key = format!("{}/{}", remote_prefix, rel);

            let local_str = if local_path.is_file() {
                Some(String::from_utf8_lossy(&read_with_retry(&local_path)?).to_string())
            } else {
                None
            };
            let remote_str = provider
                .read_bytes(&remote_key)?
                .map(|b| String::from_utf8_lossy(&b).to_string());

            match (local_str, remote_str) {
                (Some(local_raw), None) => {
                    let portable = normalize_jsonl_with_ctx(&local_raw, &ctx);
                    provider.write_bytes(&remote_key, portable.as_bytes())?;
                    self.db
                        .set_file_hash(&remote_key, &sha256_hex(portable.as_bytes()))?;
                    report.pushed += 1;
                    report.activities.push(ActivityEntry {
                        timestamp: Utc::now(),
                        kind: "push".to_string(),
                        path: remote_key,
                        detail: "Uploaded new session transcript".to_string(),
                    });
                }
                (None, Some(remote_portable)) => {
                    let local_restored = denormalize_jsonl_with_ctx(&remote_portable, &ctx);
                    atomic_write(&local_path, local_restored.as_bytes())?;
                    self.db
                        .set_file_hash(&remote_key, &sha256_hex(remote_portable.as_bytes()))?;
                    report.pulled += 1;
                    pulled_any = true;
                    report.activities.push(ActivityEntry {
                        timestamp: Utc::now(),
                        kind: "pull".to_string(),
                        path: remote_key,
                        detail: "Downloaded session transcript with path remapping".to_string(),
                    });
                }
                (Some(local_raw), Some(remote_portable)) => {
                    let local_portable = normalize_jsonl_with_ctx(&local_raw, &ctx);
                    match merge_jsonl(&local_portable, &remote_portable) {
                        MergeOutcome::Identical => {}
                        MergeOutcome::LocalAhead(merged_portable) => {
                            provider.write_bytes(&remote_key, merged_portable.as_bytes())?;
                            self.db.set_file_hash(
                                &remote_key,
                                &sha256_hex(merged_portable.as_bytes()),
                            )?;
                            report.pushed += 1;
                            report.activities.push(ActivityEntry {
                                timestamp: Utc::now(),
                                kind: "push".to_string(),
                                path: remote_key,
                                detail: "Pushed new session messages".to_string(),
                            });
                        }
                        MergeOutcome::RemoteAhead(merged_portable) => {
                            let local_restored = denormalize_jsonl_with_ctx(&merged_portable, &ctx);
                            atomic_write(&local_path, local_restored.as_bytes())?;
                            self.db.set_file_hash(
                                &remote_key,
                                &sha256_hex(merged_portable.as_bytes()),
                            )?;
                            report.pulled += 1;
                            pulled_any = true;
                            report.activities.push(ActivityEntry {
                                timestamp: Utc::now(),
                                kind: "pull".to_string(),
                                path: remote_key,
                                detail: "Pulled new session messages".to_string(),
                            });
                        }
                        MergeOutcome::Merged {
                            content,
                            local_only_count,
                            remote_only_count,
                        } => {
                            provider.write_bytes(&remote_key, content.as_bytes())?;
                            let local_restored = denormalize_jsonl_with_ctx(&content, &ctx);
                            atomic_write(&local_path, local_restored.as_bytes())?;
                            self.db
                                .set_file_hash(&remote_key, &sha256_hex(content.as_bytes()))?;
                            report.merged += 1;
                            pulled_any = true;
                            report.activities.push(ActivityEntry {
                                timestamp: Utc::now(),
                                kind: "merge".to_string(),
                                path: remote_key,
                                detail: format!(
                                    "Merged {} local + {} remote lines",
                                    local_only_count, remote_only_count
                                ),
                            });
                        }
                    }
                }
                (None, None) => {}
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

    /// Syncs `settings.json` with automatic `${SESSIONSHARE_CLAUDE_DIR}` and `${SESSIONSHARE_HOME_DIR}`
    /// path rewriting so differing `.claude` and user home paths work seamlessly across PCs.
    fn sync_settings_json(
        &self,
        provider: &dyn StorageProvider,
        report: &mut SyncReport,
    ) -> Result<()> {
        let local_path = self.config.claude_dir.join("settings.json");
        let remote_key = "global/settings.json";
        let ctx = self.rewrite_ctx(None);

        let local_str = if local_path.is_file() {
            Some(String::from_utf8_lossy(&read_with_retry(&local_path)?).to_string())
        } else {
            None
        };
        let remote_str = provider
            .read_bytes(remote_key)?
            .map(|b| String::from_utf8_lossy(&b).to_string());
        let last_hash = self.db.get_file_hash(remote_key)?;

        match (local_str, remote_str) {
            (Some(l), None) => {
                let portable = normalize_settings_json(&l, &ctx);
                let h = sha256_hex(portable.as_bytes());
                provider.write_bytes(remote_key, portable.as_bytes())?;
                self.db.set_file_hash(remote_key, &h)?;
                report.pushed += 1;
                report.activities.push(ActivityEntry {
                    timestamp: Utc::now(),
                    kind: "push".to_string(),
                    path: remote_key.to_string(),
                    detail: "Uploaded settings.json (paths normalized)".to_string(),
                });
            }
            (None, Some(r)) => {
                let restored = denormalize_settings_json(&r, &ctx);
                let h = sha256_hex(r.as_bytes());
                atomic_write(&local_path, restored.as_bytes())?;
                self.db.set_file_hash(remote_key, &h)?;
                report.pulled += 1;
                report.activities.push(ActivityEntry {
                    timestamp: Utc::now(),
                    kind: "pull".to_string(),
                    path: remote_key.to_string(),
                    detail: "Downloaded settings.json (paths remapped)".to_string(),
                });
            }
            (Some(l), Some(r)) => {
                let local_portable = normalize_settings_json(&l, &ctx);
                let lh = sha256_hex(local_portable.as_bytes());
                let rh = sha256_hex(r.as_bytes());
                if lh == rh {
                    if last_hash.as_deref() != Some(lh.as_str()) {
                        self.db.set_file_hash(remote_key, &lh)?;
                    }
                    return Ok(());
                }

                if last_hash.as_deref() == Some(rh.as_str()) {
                    provider.write_bytes(remote_key, local_portable.as_bytes())?;
                    self.db.set_file_hash(remote_key, &lh)?;
                    report.pushed += 1;
                } else if last_hash.as_deref() == Some(lh.as_str()) {
                    let restored = denormalize_settings_json(&r, &ctx);
                    atomic_write(&local_path, restored.as_bytes())?;
                    self.db.set_file_hash(remote_key, &rh)?;
                    report.pulled += 1;
                } else {
                    let local_mtime = fs::metadata(&local_path).ok().and_then(|m| m.modified().ok());
                    let remote_mtime = provider.modified_time(remote_key);
                    let conflict_backup = format!(
                        "{}.conflict.{}",
                        local_path.display(),
                        Utc::now().format("%Y%m%d%H%M%S")
                    );

                    if local_mtime >= remote_mtime {
                        let backup_restored = denormalize_settings_json(&r, &ctx);
                        atomic_write(Path::new(&conflict_backup), backup_restored.as_bytes())?;
                        provider.write_bytes(remote_key, local_portable.as_bytes())?;
                        self.db.set_file_hash(remote_key, &lh)?;
                        report.pushed += 1;
                    } else {
                        atomic_write(Path::new(&conflict_backup), l.as_bytes())?;
                        let restored = denormalize_settings_json(&r, &ctx);
                        atomic_write(&local_path, restored.as_bytes())?;
                        self.db.set_file_hash(remote_key, &rh)?;
                        report.pulled += 1;
                    }
                    report.conflicts.push(remote_key.to_string());
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
        let remote_key = "global/history.jsonl";

        let local_str = if local_path.is_file() {
            Some(String::from_utf8_lossy(&read_with_retry(&local_path)?).to_string())
        } else {
            None
        };
        let remote_str = provider
            .read_bytes(remote_key)?
            .map(|b| String::from_utf8_lossy(&b).to_string());

        let map = &self.config.local_project_paths;
        match (local_str, remote_str) {
            (Some(local_raw), None) => {
                let portable = normalize_history_jsonl(&local_raw, map);
                provider.write_bytes(remote_key, portable.as_bytes())?;
                report.pushed += 1;
                report.activities.push(ActivityEntry {
                    timestamp: Utc::now(),
                    kind: "push".to_string(),
                    path: remote_key.to_string(),
                    detail: "Uploaded global history.jsonl".to_string(),
                });
            }
            (None, Some(remote_portable)) => {
                let restored = denormalize_history_jsonl(&remote_portable, map);
                atomic_write(&local_path, restored.as_bytes())?;
                report.pulled += 1;
                report.activities.push(ActivityEntry {
                    timestamp: Utc::now(),
                    kind: "pull".to_string(),
                    path: remote_key.to_string(),
                    detail: "Downloaded global history.jsonl".to_string(),
                });
            }
            (Some(local_raw), Some(remote_portable)) => {
                let local_portable = normalize_history_jsonl(&local_raw, map);
                match merge_jsonl(&local_portable, &remote_portable) {
                    MergeOutcome::Identical => {}
                    MergeOutcome::LocalAhead(merged) => {
                        provider.write_bytes(remote_key, merged.as_bytes())?;
                        report.pushed += 1;
                    }
                    MergeOutcome::RemoteAhead(merged) => {
                        let restored = denormalize_history_jsonl(&merged, map);
                        atomic_write(&local_path, restored.as_bytes())?;
                        report.pulled += 1;
                    }
                    MergeOutcome::Merged { content, .. } => {
                        provider.write_bytes(remote_key, content.as_bytes())?;
                        let restored = denormalize_history_jsonl(&content, map);
                        atomic_write(&local_path, restored.as_bytes())?;
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
    ) -> Result<()> {
        let mut rel_files: BTreeSet<String> = BTreeSet::new();

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
                    rel_files.insert(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }

        for remote_full in provider.list_files(remote_prefix)? {
            if let Some(rel) = remote_full.strip_prefix(&format!("{}/", remote_prefix)) {
                if !rel.contains(".conflict.") {
                    rel_files.insert(rel.to_string());
                }
            }
        }

        for rel in rel_files {
            let local_file = local_dir.join(rel.replace('/', "\\"));
            let remote_key = format!("{}/{}", remote_prefix, rel);
            self.sync_generic_file(provider, &local_file, &remote_key, report)?;
        }

        Ok(())
    }

    fn sync_generic_file(
        &self,
        provider: &dyn StorageProvider,
        local_path: &Path,
        remote_key: &str,
        report: &mut SyncReport,
    ) -> Result<()> {
        let local_bytes = if local_path.is_file() {
            Some(read_with_retry(local_path)?)
        } else {
            None
        };
        let remote_bytes = provider.read_bytes(remote_key)?;
        let last_hash = self.db.get_file_hash(remote_key)?;

        match (local_bytes, remote_bytes) {
            (Some(l), None) => {
                let h = sha256_hex(&l);
                provider.write_bytes(remote_key, &l)?;
                self.db.set_file_hash(remote_key, &h)?;
                report.pushed += 1;
                report.activities.push(ActivityEntry {
                    timestamp: Utc::now(),
                    kind: "push".to_string(),
                    path: remote_key.to_string(),
                    detail: "Uploaded file".to_string(),
                });
            }
            (None, Some(r)) => {
                let h = sha256_hex(&r);
                atomic_write(local_path, &r)?;
                self.db.set_file_hash(remote_key, &h)?;
                report.pulled += 1;
                report.activities.push(ActivityEntry {
                    timestamp: Utc::now(),
                    kind: "pull".to_string(),
                    path: remote_key.to_string(),
                    detail: "Downloaded file".to_string(),
                });
            }
            (Some(l), Some(r)) => {
                let lh = sha256_hex(&l);
                let rh = sha256_hex(&r);
                if lh == rh {
                    if last_hash.as_deref() != Some(lh.as_str()) {
                        self.db.set_file_hash(remote_key, &lh)?;
                    }
                    return Ok(());
                }

                if last_hash.as_deref() == Some(rh.as_str()) {
                    provider.write_bytes(remote_key, &l)?;
                    self.db.set_file_hash(remote_key, &lh)?;
                    report.pushed += 1;
                    report.activities.push(ActivityEntry {
                        timestamp: Utc::now(),
                        kind: "push".to_string(),
                        path: remote_key.to_string(),
                        detail: "Pushed updated file".to_string(),
                    });
                } else if last_hash.as_deref() == Some(lh.as_str()) {
                    atomic_write(local_path, &r)?;
                    self.db.set_file_hash(remote_key, &rh)?;
                    report.pulled += 1;
                    report.activities.push(ActivityEntry {
                        timestamp: Utc::now(),
                        kind: "pull".to_string(),
                        path: remote_key.to_string(),
                        detail: "Pulled updated file".to_string(),
                    });
                } else {
                    let local_mtime = fs::metadata(local_path).ok().and_then(|m| m.modified().ok());
                    let remote_mtime = provider.modified_time(remote_key);
                    let conflict_backup = format!(
                        "{}.conflict.{}",
                        local_path.display(),
                        Utc::now().format("%Y%m%d%H%M%S")
                    );

                    if local_mtime >= remote_mtime {
                        atomic_write(Path::new(&conflict_backup), &r)?;
                        provider.write_bytes(remote_key, &l)?;
                        self.db.set_file_hash(remote_key, &lh)?;
                        report.pushed += 1;
                    } else {
                        atomic_write(Path::new(&conflict_backup), &l)?;
                        atomic_write(local_path, &r)?;
                        self.db.set_file_hash(remote_key, &rh)?;
                        report.pulled += 1;
                    }
                    report.conflicts.push(remote_key.to_string());
                    report.activities.push(ActivityEntry {
                        timestamp: Utc::now(),
                        kind: "conflict".to_string(),
                        path: remote_key.to_string(),
                        detail: format!(
                            "Conflict resolved (newest kept; backup at {})",
                            conflict_backup
                        ),
                    });
                }
            }
            (None, None) => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        let rep_a = engine_a.sync_all().unwrap();
        assert!(rep_a.pushed >= 3);

        // Machine B: Different Windows username AND custom Claude config dir, AND Claude has never run yet!
        let home_b = tmp.path().join("Users").join("OfficeUser");
        let claude_b = tmp.path().join("CustomOfficeClaude");
        let repo_b = home_b.join("Dev").join("Portal");
        fs::create_dir_all(&repo_b).unwrap();
        let repo_b_str = normalize_windows_path(&repo_b.to_string_lossy());

        let mut cfg_b = AppConfig::new_default(claude_b.clone());
        cfg_b.machine_name = "Office-Laptop".to_string();
        cfg_b.sync_dir = Some(sync_drive.clone());
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
}

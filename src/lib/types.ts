export interface GlobalSyncOptions {
  skills: boolean;
  agents: boolean;
  commands: boolean;
  plans: boolean;
  rules: boolean;
  hooks: boolean;
  claude_md: boolean;
  settings_json: boolean;
  history_jsonl: boolean;
  project_memory: boolean;
  desktop_sessions: boolean;
}

export interface AppConfig {
  machine_id: string;
  machine_name: string;
  claude_dir: string;
  claude_desktop_dir: string | null;
  sync_dir: string | null;
  auto_sync: boolean;
  debounce_secs: number;
  workspace_roots: string[];
  global_options: GlobalSyncOptions;
  project_sync_enabled: Record<string, boolean>;
  local_project_paths: Record<string, string>;
  mapping_sources: Record<string, string>;
  periodic_sync_mins: number;
  active_session_grace_secs: number;
  propagate_deletions: boolean;
  require_hook_approval: boolean;
  settings_excluded_keys: string[];
  retention_days: number;
  launch_at_login: boolean;
}

export interface SettingsPayload {
  machine_name: string;
  claude_dir: string;
  claude_desktop_dir: string | null;
  sync_dir: string | null;
  auto_sync: boolean;
  debounce_secs: number;
  workspace_roots: string[];
  global_options: GlobalSyncOptions;
  periodic_sync_mins: number;
  active_session_grace_secs: number;
  propagate_deletions: boolean;
  require_hook_approval: boolean;
  settings_excluded_keys: string[];
  retention_days: number;
  launch_at_login: boolean;
}

export interface ProjectView {
  canonical_id: string;
  display_name: string;
  git_remote: string | null;
  local_path: string | null;
  encoded_local_dir: string | null;
  mapping_source: string | null;
  remote_machines: string[];
  sync_enabled: boolean;
  local_session_count: number;
  remote_session_count: number;
  needs_mapping: boolean;
}

export interface SessionSummary {
  session_id: string;
  project_id: string;
  project_name: string;
  local_project_path: string | null;
  title: string;
  last_modified: string | null;
  line_count: number;
  size_bytes: number;
  in_local: boolean;
  in_remote: boolean;
}

export interface ActivityEntry {
  timestamp: string;
  kind: string;
  path: string;
  detail: string;
}

export interface SyncProgress {
  phase: string;
  current_item: string | null;
  checked: number;
  completed: number;
  total: number;
  upcoming: string[];
  recent: ActivityEntry[];
}

export interface SyncReport {
  timestamp: string;
  pushed: number;
  pulled: number;
  merged: number;
  auto_mapped: number;
  deleted: number;
  deferred: number;
  pending_approvals: number;
  unresolved_conflicts: number;
  conflicts: string[];
  errors: string[];
  activities: ActivityEntry[];
}

export interface SyncStatus {
  configured: boolean;
  auto_sync: boolean;
  is_syncing: boolean;
  machine_name: string;
  last_report: SyncReport | null;
  sync_generation: number;
  progress: SyncProgress | null;
  watcher_active: boolean;
  watcher_error: string | null;
  pending_approvals: number;
}

export interface PendingApproval {
  remote_key: string;
  remote_hash: string;
  detected_at: string;
  summary: string;
  preview: string;
}

export interface MergeConflict {
  remote_key: string;
  local_path: string;
  local_hash: string;
  remote_hash: string;
  has_base: boolean;
  detected_at: string;
}

export interface Chunk {
  stable: boolean;
  base: string[] | null;
  local: string[];
  remote: string[];
  auto: "local" | "remote" | null;
}

export interface ConflictView {
  remote_key: string;
  local_path: string;
  kind: "text" | "json";
  has_base: boolean;
  local_hash: string;
  remote_hash: string;
  local_text: string;
  remote_text: string;
  chunks: Chunk[];
}

export interface Overview {
  projects: ProjectView[];
  sessions: SessionSummary[];
  activities: ActivityEntry[];
  pending_approvals: PendingApproval[];
  conflicts: MergeConflict[];
}

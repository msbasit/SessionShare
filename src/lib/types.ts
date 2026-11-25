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
}

export interface AppConfig {
  machine_id: string;
  machine_name: string;
  claude_dir: string;
  sync_dir: string | null;
  auto_sync: boolean;
  debounce_secs: number;
  workspace_roots: string[];
  global_options: GlobalSyncOptions;
  project_sync_enabled: Record<string, boolean>;
  local_project_paths: Record<string, string>;
  mapping_sources: Record<string, string>;
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

export interface SyncReport {
  timestamp: string;
  pushed: number;
  pulled: number;
  merged: number;
  auto_mapped: number;
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
}

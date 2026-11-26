import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  ActivityEntry,
  AppConfig,
  ConflictView,
  Overview,
  PendingApproval,
  ProjectView,
  SessionSummary,
  SettingsPayload,
  SyncReport,
  SyncStatus,
} from "./types";

/** Builds a full settings payload from the current config, with optional overrides. */
export function settingsPayload(cfg: AppConfig, overrides: Partial<SettingsPayload> = {}): SettingsPayload {
  return {
    machine_name: cfg.machine_name,
    claude_dir: cfg.claude_dir,
    claude_desktop_dir: cfg.claude_desktop_dir ?? null,
    sync_dir: cfg.sync_dir,
    auto_sync: cfg.auto_sync,
    debounce_secs: cfg.debounce_secs,
    workspace_roots: cfg.workspace_roots ?? [],
    global_options: cfg.global_options,
    periodic_sync_mins: cfg.periodic_sync_mins,
    active_session_grace_secs: cfg.active_session_grace_secs,
    propagate_deletions: cfg.propagate_deletions,
    require_hook_approval: cfg.require_hook_approval,
    settings_excluded_keys: cfg.settings_excluded_keys ?? [],
    retention_days: cfg.retention_days,
    launch_at_login: cfg.launch_at_login,
    ...overrides,
  };
}

export const api = {
  getConfig: () => invoke<AppConfig>("get_config"),
  getSyncStatus: () => invoke<SyncStatus>("get_sync_status"),
  saveSettings: (payload: SettingsPayload) => invoke<AppConfig>("save_settings", { payload }),
  getOverview: () => invoke<Overview>("get_overview"),
  listProjects: () => invoke<ProjectView[]>("list_projects"),
  mapProject: (canonicalId: string, localPath: string) =>
    invoke<void>("map_project", { canonicalId, localPath }),
  toggleProjectSync: (canonicalId: string, enabled: boolean) =>
    invoke<void>("toggle_project_sync", { canonicalId, enabled }),
  listSessions: () => invoke<SessionSummary[]>("list_sessions"),
  runSyncNow: () => invoke<SyncReport>("run_sync_now"),
  getRecentActivities: () => invoke<ActivityEntry[]>("get_recent_activities"),
  listPendingApprovals: () => invoke<PendingApproval[]>("list_pending_approvals"),
  resolveApproval: (remoteKey: string, remoteHash: string, approve: boolean) =>
    invoke<void>("resolve_approval", { remoteKey, remoteHash, approve }),
  getConflictView: (remoteKey: string) => invoke<ConflictView>("get_conflict_view", { remoteKey }),
  resolveConflict: (remoteKey: string, localHash: string, remoteHash: string, content: string) =>
    invoke<void>("resolve_conflict", { remoteKey, localHash, remoteHash, content }),
  pickFolder: async (title: string): Promise<string | null> => {
    const selected = await open({
      directory: true,
      multiple: false,
      title,
    });
    return typeof selected === "string" ? selected : null;
  },
};

import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  ActivityEntry,
  AppConfig,
  GlobalSyncOptions,
  ProjectView,
  SessionSummary,
  SyncReport,
  SyncStatus,
} from "./types";

export const api = {
  getConfig: () => invoke<AppConfig>("get_config"),
  getSyncStatus: () => invoke<SyncStatus>("get_sync_status"),
  saveSettings: (payload: {
    machine_name: string;
    claude_dir: string;
    sync_dir: string | null;
    auto_sync: boolean;
    debounce_secs: number;
    workspace_roots: string[];
    global_options: GlobalSyncOptions;
  }) => invoke<AppConfig>("save_settings", { payload }),
  listProjects: () => invoke<ProjectView[]>("list_projects"),
  mapProject: (canonicalId: string, localPath: string) =>
    invoke<void>("map_project", { canonicalId, localPath }),
  toggleProjectSync: (canonicalId: string, enabled: boolean) =>
    invoke<void>("toggle_project_sync", { canonicalId, enabled }),
  listSessions: () => invoke<SessionSummary[]>("list_sessions"),
  runSyncNow: () => invoke<SyncReport>("run_sync_now"),
  getRecentActivities: () => invoke<ActivityEntry[]>("get_recent_activities"),
  pickFolder: async (title: string): Promise<string | null> => {
    const selected = await open({
      directory: true,
      multiple: false,
      title,
    });
    return typeof selected === "string" ? selected : null;
  },
};

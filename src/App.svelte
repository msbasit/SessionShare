<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { api, settingsPayload } from "./lib/api";
  import type {
    AppConfig,
    GlobalSyncOptions,
    Overview,
    PendingApproval,
    ProjectView,
    SettingsPayload,
    SyncReport,
    SyncProgress as SyncProgressData,
    SyncStatus,
  } from "./lib/types";
  import { formatDate } from "./lib/format";
  import ProjectsTab from "./components/ProjectsTab.svelte";
  import SessionsTab from "./components/SessionsTab.svelte";
  import GlobalTab from "./components/GlobalTab.svelte";
  import ActivityTab from "./components/ActivityTab.svelte";
  import ReviewTab from "./components/ReviewTab.svelte";
  import SettingsTab from "./components/SettingsTab.svelte";
  import SyncProgress from "./components/SyncProgress.svelte";

  type Tab = "projects" | "sessions" | "global" | "review" | "activity" | "settings";
  const emptyOverview: Overview = {
    projects: [],
    sessions: [],
    activities: [],
    pending_approvals: [],
    conflicts: [],
  };

  let activeTab = $state<Tab>("projects");
  let config = $state<AppConfig | null>(null);
  let status = $state<SyncStatus | null>(null);
  let overview = $state<Overview>(emptyOverview);
  let syncing = $state(false);
  let syncProgress = $state<SyncProgressData | null>(null);
  let syncGeneration = 0;
  let finishedGeneration = 0;
  let sawSyncEvent = false;
  let toast = $state<{ kind: "ok" | "err"; text: string } | null>(null);
  let maximized = $state(false);

  async function windowAction(action: "minimize" | "toggleMaximize" | "close") {
    try {
      await getCurrentWindow()[action]();
    } catch (err) {
      showError(`Window action failed: ${String(err)}`);
    }
  }

  function showToast(kind: "ok" | "err", text: string) {
    toast = { kind, text };
    setTimeout(() => {
      if (toast?.text === text) toast = null;
    }, 4000);
  }

  function showError(message: string) {
    showToast("err", message);
  }

  let refreshing = false;
  let refreshQueued = false;

  /** Reloads everything; overlapping calls are coalesced into one follow-up refresh. */
  async function refreshAll() {
    if (refreshing) {
      refreshQueued = true;
      return;
    }
    refreshing = true;
    try {
      const [cfg, st] = await Promise.all([api.getConfig(), api.getSyncStatus()]);
      config = cfg;
      status = st;
      if (st.sync_generation > syncGeneration || (st.sync_generation === syncGeneration && !sawSyncEvent)) {
        syncGeneration = st.sync_generation;
        sawSyncEvent = false;
        syncing = st.is_syncing;
        syncProgress = st.progress;
        if (!st.is_syncing) finishedGeneration = Math.max(finishedGeneration, st.sync_generation);
      }
      if (cfg.sync_dir) {
        overview = await api.getOverview();
      } else {
        overview = emptyOverview;
        activeTab = "settings";
      }
    } catch (err) {
      showError(String(err));
    } finally {
      refreshing = false;
      if (refreshQueued) {
        refreshQueued = false;
        refreshAll();
      }
    }
  }

  function summarize(report: SyncReport): string {
    const parts = [
      `${report.pushed} pushed`,
      `${report.pulled} pulled`,
      `${report.merged} merged`,
    ];
    if (report.deleted) parts.push(`${report.deleted} deleted`);
    if (report.deferred) parts.push(`${report.deferred} deferred (in use)`);
    if (report.auto_mapped) parts.push(`${report.auto_mapped} auto-mapped`);
    if (report.unresolved_conflicts) parts.push(`${report.unresolved_conflicts} to merge`);
    if (report.pending_approvals) parts.push(`${report.pending_approvals} awaiting review`);
    let text = `Synced: ${parts.join(", ")}`;
    if (report.errors.length) text += ` — ${report.errors.length} error(s): ${report.errors[0]}`;
    return text;
  }

  async function handleSyncNow() {
    if (syncing) return;
    syncing = true;
    try {
      const report = await api.runSyncNow();
      showToast(report.errors.length ? "err" : "ok", summarize(report));
    } catch (err) {
      showError(String(err));
    } finally {
      await refreshAll();
    }
  }

  async function handleSaveSettings(payload: SettingsPayload) {
    try {
      config = await api.saveSettings(payload);
      showToast("ok", "Settings saved");
      await refreshAll();
      if (config.sync_dir) activeTab = "projects";
    } catch (err) {
      showError(String(err));
      await refreshAll();
    }
  }

  async function handleToggleGlobal(key: keyof GlobalSyncOptions) {
    if (!config) return;
    const global_options = { ...config.global_options, [key]: !config.global_options[key] };
    try {
      config = await api.saveSettings(settingsPayload(config, { global_options }));
      showToast("ok", "Updated global sync preferences");
    } catch (err) {
      showError(String(err));
    }
  }

  async function handleMapProject(proj: ProjectView) {
    const picked = await api.pickFolder(`Select local folder for "${proj.display_name}"`);
    if (!picked) return;
    try {
      await api.mapProject(proj.canonical_id, picked);
      showToast("ok", `Mapped ${proj.display_name} -> ${picked}`);
      await refreshAll();
    } catch (err) {
      showError(String(err));
    }
  }

  async function handleToggleProject(proj: ProjectView) {
    try {
      await api.toggleProjectSync(proj.canonical_id, !proj.sync_enabled);
      await refreshAll();
    } catch (err) {
      showError(String(err));
    }
  }

  async function handleResolve(item: PendingApproval, approve: boolean) {
    try {
      await api.resolveApproval(item.remote_key, item.remote_hash, approve);
      showToast(
        "ok",
        approve ? `Approved ${item.remote_key}; applying on sync` : `Rejected ${item.remote_key}`
      );
      await refreshAll();
    } catch (err) {
      showError(String(err));
    }
  }

  let pendingCount = $derived(overview.pending_approvals.length + overview.conflicts.length);

  onMount(() => {
    const appWindow = getCurrentWindow();
    const updateMaximized = async () => {
      try {
        maximized = await appWindow.isMaximized();
      } catch (err) {
        showError(`Could not read window state: ${String(err)}`);
      }
    };
    updateMaximized();
    const unlistenResize = appWindow.onResized(updateMaximized);
    const unlistenStart = listen<number>("sync-started", (ev) => {
      if (ev.payload < syncGeneration || ev.payload <= finishedGeneration) return;
      if (ev.payload === syncGeneration && syncing && syncProgress) return;
      syncGeneration = ev.payload;
      sawSyncEvent = true;
      syncing = true;
      syncProgress = null;
    });
    const unlistenProgress = listen<{ generation: number; progress: SyncProgressData }>("sync-progress", (ev) => {
      if (ev.payload.generation < syncGeneration || ev.payload.generation <= finishedGeneration) return;
      syncGeneration = ev.payload.generation;
      sawSyncEvent = true;
      syncing = true;
      syncProgress = ev.payload.progress;
    });
    const unlistenDone = listen<{ generation: number; report: SyncReport }>("sync-completed", (ev) => {
      if (ev.payload.generation < syncGeneration) return;
      syncGeneration = ev.payload.generation;
      finishedGeneration = ev.payload.generation;
      sawSyncEvent = true;
      syncing = false;
      syncProgress = null;
      refreshAll();
    });
    const unlistenErr = listen<{ generation: number; error: string }>("sync-error", (ev) => {
      if (ev.payload.generation < syncGeneration) return;
      syncGeneration = ev.payload.generation;
      finishedGeneration = ev.payload.generation;
      sawSyncEvent = true;
      syncing = false;
      syncProgress = null;
      showError(ev.payload.error);
    });
    Promise.all([unlistenStart, unlistenProgress, unlistenDone, unlistenErr])
      .catch((err) => showError(`Could not monitor sync: ${String(err)}`))
      .finally(() => refreshAll());

    return () => {
      unlistenStart.then((fn) => fn());
      unlistenProgress.then((fn) => fn());
      unlistenDone.then((fn) => fn());
      unlistenErr.then((fn) => fn());
      unlistenResize.then((fn) => fn()).catch((err) => showError(`Could not monitor window state: ${String(err)}`));
    };
  });
</script>

<div class="shell">
  <div class="titlebar" data-tauri-drag-region>
    <div class="titlebar-name" data-tauri-drag-region>
      <svg class="titlebar-mark" viewBox="0 0 24 24" fill="none" aria-hidden="true">
        <path d="M5 8.5h11.5l-3-3M19 15.5H7.5l3 3" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />
      </svg>
      <span data-tauri-drag-region>SessionShare</span>
    </div>
    <span class="titlebar-caption" data-tauri-drag-region>Claude Code sync</span>
    <div class="window-controls">
      <button class="window-control" aria-label="Minimize" title="Minimize" onclick={() => windowAction("minimize")}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 8h10" /></svg>
      </button>
      <button class="window-control" aria-label={maximized ? "Restore" : "Maximize"} title={maximized ? "Restore" : "Maximize"} onclick={() => windowAction("toggleMaximize")}>
        {#if maximized}
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M5 4V2.5h8.5V11H12M2.5 5.5H11V14H2.5z" /></svg>
        {:else}
          <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 3h10v10H3z" /></svg>
        {/if}
      </button>
      <button class="window-control close" aria-label="Close to system tray" title="Close to system tray" onclick={() => windowAction("close")}>
        <svg viewBox="0 0 16 16" aria-hidden="true"><path d="m3.5 3.5 9 9m0-9-9 9" /></svg>
      </button>
    </div>
  </div>
  <header class="topbar">
    <div class="brand">
      <div class="brand-mark" aria-hidden="true">
        <svg viewBox="0 0 24 24" fill="none"><path d="M4 8h13l-3.5-3.5M20 16H7l3.5 3.5" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </div>
      <div>
        <h1>SessionShare</h1>
        <span class="subtitle">
          {status?.machine_name ?? "Windows PC"} <span class="separator">/</span> Local synced drive
          {#if status?.last_report}
            · Last sync {formatDate(status.last_report.timestamp)}
          {/if}
        </span>
      </div>
    </div>

    <div class="topbar-actions">
      {#if status?.configured}
        {#if status.watcher_error}
          <span class="pill err" title={status.watcher_error}>Watcher error</span>
        {/if}
        <span class="pill" class:active={status.auto_sync}>
          {status.auto_sync ? "Auto-Sync On" : "Auto-Sync Paused"}
        </span>
        <button class="btn primary" onclick={handleSyncNow} disabled={syncing}>
          {syncing ? "Syncing..." : "Sync Now"}
        </button>
      {:else}
        <span class="pill warn">Setup Required</span>
      {/if}
    </div>
  </header>

  <nav class="tabs" aria-label="Main navigation">
    <button class:active={activeTab === "projects"} onclick={() => (activeTab = "projects")}>
      Projects ({overview.projects.length})
    </button>
    <button class:active={activeTab === "sessions"} onclick={() => (activeTab = "sessions")}>
      Sessions ({overview.sessions.length})
    </button>
    <button class:active={activeTab === "global"} onclick={() => (activeTab = "global")}>
      Global Assets
    </button>
    <button class:active={activeTab === "review"} onclick={() => (activeTab = "review")}>
      Review
      {#if pendingCount > 0}<span class="badge warn">{pendingCount}</span>{/if}
    </button>
    <button class:active={activeTab === "activity"} onclick={() => (activeTab = "activity")}>
      Activity ({overview.activities.length})
    </button>
    <button class:active={activeTab === "settings"} onclick={() => (activeTab = "settings")}>
      Settings
    </button>
  </nav>

  {#if toast}
    <div class="toast" class:err={toast.kind === "err"}>{toast.text}</div>
  {/if}

  {#if pendingCount > 0 && activeTab !== "review"}
    <button class="banner" onclick={() => (activeTab = "review")}>
      {pendingCount} item(s) need your review: files changed on both PCs, or incoming hook / permission changes.
    </button>
  {/if}

  <main class="content">
    {#if syncing}
      <SyncProgress progress={syncProgress} />
    {/if}
    {#if !config}
      <div class="empty-card"><p>Loading...</p></div>
    {:else if !config.sync_dir && activeTab !== "settings"}
      <div class="empty-card">
        <h2>Choose your Synced Drive folder</h2>
        <p>
          Point SessionShare to a folder inside your approved OneDrive / Google Drive / Dropbox
          directory on both laptops. SessionShare makes zero outbound network calls itself.
        </p>
        <button class="btn primary" onclick={() => (activeTab = "settings")}>Open Setup</button>
      </div>
    {:else if activeTab === "projects"}
      <ProjectsTab
        projects={overview.projects}
        onRefresh={refreshAll}
        onMap={handleMapProject}
        onToggle={handleToggleProject}
      />
    {:else if activeTab === "sessions"}
      <SessionsTab sessions={overview.sessions} onError={showError} />
    {:else if activeTab === "global"}
      <GlobalTab options={config.global_options} onToggle={handleToggleGlobal} />
    {:else if activeTab === "review"}
      <ReviewTab
        pending={overview.pending_approvals}
        conflicts={overview.conflicts}
        activities={overview.activities}
        onResolve={handleResolve}
        onChanged={() => {
          showToast("ok", "Merged version saved on both PCs");
          refreshAll();
        }}
        onError={showError}
      />
    {:else if activeTab === "activity"}
      <ActivityTab activities={overview.activities} onRefresh={refreshAll} />
    {:else if activeTab === "settings"}
      <SettingsTab {config} onSave={handleSaveSettings} />
    {/if}
  </main>
</div>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  .titlebar {
    height: 38px;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    background: var(--titlebar);
    border-bottom: 1px solid var(--border);
    user-select: none;
  }

  .titlebar-name {
    display: flex;
    align-items: center;
    gap: 8px;
    padding-left: 16px;
    font-size: 12px;
    font-weight: 650;
    color: var(--text);
    pointer-events: none;
  }

  .titlebar-mark {
    width: 18px;
    height: 18px;
    color: var(--accent);
  }

  .titlebar-caption {
    flex: 1;
    text-align: center;
    color: var(--muted);
    font-size: 11px;
    letter-spacing: 0.02em;
  }

  .window-controls {
    display: flex;
    align-self: stretch;
  }

  .window-control {
    width: 46px;
    display: grid;
    place-items: center;
    padding: 0;
    border: 0;
    border-radius: 0;
    color: var(--text);
    background: transparent;
  }

  .window-control:hover {
    background: var(--surface-hover);
  }

  .window-control.close:hover {
    background: #c42b1c;
    color: #fff;
  }

  .window-control svg {
    width: 16px;
    height: 16px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.15;
  }

  .topbar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
    padding: 20px 28px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .brand-mark {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: 12px;
    background: var(--accent-soft);
    color: var(--accent);
  }

  .brand-mark svg {
    width: 23px;
    height: 23px;
  }

  .brand h1 {
    margin: 0;
    font-size: 17px;
    font-weight: 700;
    letter-spacing: -0.02em;
  }

  .subtitle {
    font-size: 12px;
    color: var(--muted);
  }

  .separator {
    color: var(--border-strong);
    padding: 0 3px;
  }

  .topbar-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .tabs {
    display: flex;
    gap: 4px;
    padding: 0 28px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }

  .tabs button {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--muted);
    padding: 12px 14px;
    font-size: 13px;
    font-weight: 500;
  }

  .tabs button.active {
    color: var(--text);
    border-bottom-color: var(--accent);
  }

  .content {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 28px;
  }

  .toast,
  .banner {
    margin: 10px 28px 0;
    padding: 9px 14px;
    border-radius: 8px;
    font-size: 13px;
    text-align: left;
  }

  .toast {
    background: var(--success-soft);
    border: 1px solid var(--success-border);
    color: var(--success);
  }

  .toast.err {
    background: var(--danger-soft);
    border-color: var(--danger-border);
    color: var(--danger);
  }

  .banner {
    background: var(--warning-soft);
    border: 1px solid var(--warning-border);
    color: var(--warning);
  }
</style>

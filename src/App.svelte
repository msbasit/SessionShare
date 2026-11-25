<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import { api } from "./lib/api";
  import type {
    ActivityEntry,
    AppConfig,
    ProjectView,
    SessionSummary,
    SyncReport,
    SyncStatus,
  } from "./lib/types";

  type Tab = "projects" | "sessions" | "global" | "activity" | "settings";

  let activeTab = $state<Tab>("projects");
  let config = $state<AppConfig | null>(null);
  let status = $state<SyncStatus | null>(null);
  let projects = $state<ProjectView[]>([]);
  let sessions = $state<SessionSummary[]>([]);
  let activities = $state<ActivityEntry[]>([]);
  let searchQuery = $state("");
  let copiedId = $state<string | null>(null);
  let syncing = $state(false);
  let toast = $state<{ kind: "ok" | "err"; text: string } | null>(null);

  // Settings form state
  let machineName = $state("");
  let claudeDir = $state("");
  let syncDir = $state("");
  let autoSync = $state(true);
  let debounceSecs = $state(6);
  let workspaceRoots = $state<string[]>([]);

  function showToast(kind: "ok" | "err", text: string) {
    toast = { kind, text };
    setTimeout(() => {
      if (toast?.text === text) toast = null;
    }, 3500);
  }

  async function refreshAll() {
    try {
      const [cfg, st] = await Promise.all([api.getConfig(), api.getSyncStatus()]);
      config = cfg;
      status = st;
      machineName = cfg.machine_name;
      claudeDir = cfg.claude_dir;
      syncDir = cfg.sync_dir ?? "";
      autoSync = cfg.auto_sync;
      debounceSecs = cfg.debounce_secs;
      workspaceRoots = [...(cfg.workspace_roots ?? [])];

      if (cfg.sync_dir) {
        const [projs, sess, acts] = await Promise.all([
          api.listProjects(),
          api.listSessions(),
          api.getRecentActivities(),
        ]);
        projects = projs;
        sessions = sess;
        activities = acts;
      } else {
        activeTab = "settings";
      }
    } catch (err) {
      showToast("err", String(err));
    }
  }

  async function handleSyncNow() {
    if (syncing) return;
    syncing = true;
    try {
      const report = await api.runSyncNow();
      const autoNote = report.auto_mapped > 0 ? `, ${report.auto_mapped} auto-mapped` : "";
      showToast(
        report.errors.length ? "err" : "ok",
        `Synced: ${report.pushed} pushed, ${report.pulled} pulled, ${report.merged} merged${autoNote}`
      );
      await refreshAll();
    } catch (err) {
      showToast("err", String(err));
    } finally {
      syncing = false;
    }
  }

  async function handleSaveSettings() {
    if (!config) return;
    try {
      config = await api.saveSettings({
        machine_name: machineName,
        claude_dir: claudeDir,
        sync_dir: syncDir.trim() || null,
        auto_sync: autoSync,
        debounce_secs: Number(debounceSecs) || 6,
        workspace_roots: workspaceRoots,
        global_options: config.global_options,
      });
      showToast("ok", "Settings saved");
      await refreshAll();
      if (config.sync_dir) {
        activeTab = "projects";
      }
    } catch (err) {
      showToast("err", String(err));
    }
  }

  async function handlePickSyncDir() {
    const picked = await api.pickFolder("Select your approved Synced Drive folder (e.g. OneDrive\\SessionShare)");
    if (picked) {
      syncDir = picked;
    }
  }

  async function handlePickClaudeDir() {
    const picked = await api.pickFolder("Select your local .claude folder");
    if (picked) {
      claudeDir = picked;
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
      showToast("err", String(err));
    }
  }

  async function handleToggleProject(proj: ProjectView) {
    try {
      await api.toggleProjectSync(proj.canonical_id, !proj.sync_enabled);
      await refreshAll();
    } catch (err) {
      showToast("err", String(err));
    }
  }

  async function handleAddWorkspaceRoot() {
    const picked = await api.pickFolder("Select a workspace root folder (e.g. C:\\Dev or D:\\Work)");
    if (picked && !workspaceRoots.some((r) => r.toLowerCase() === picked.toLowerCase())) {
      workspaceRoots = [...workspaceRoots, picked];
    }
  }

  function handleRemoveWorkspaceRoot(root: string) {
    workspaceRoots = workspaceRoots.filter((r) => r !== root);
  }

  function formatMatchSource(src: string | null): string {
    switch (src) {
      case "git":
        return "Auto: Git Remote";
      case "home-relative":
        return "Auto: Home Path";
      case "workspace-scan":
        return "Auto: Workspace Scan";
      case "folder-name":
        return "Auto: Folder Name";
      case "manual":
        return "Manual Map";
      default:
        return "";
    }
  }

  async function handleToggleGlobal(key: keyof AppConfig["global_options"]) {
    if (!config) return;
    const nextOpts = { ...config.global_options, [key]: !config.global_options[key] };
    config = await api.saveSettings({
      machine_name: config.machine_name,
      claude_dir: config.claude_dir,
      sync_dir: config.sync_dir,
      auto_sync: config.auto_sync,
      debounce_secs: config.debounce_secs,
      workspace_roots: config.workspace_roots ?? [],
      global_options: nextOpts,
    });
    showToast("ok", "Updated global sync preferences");
  }

  async function copyResumeCommand(sess: SessionSummary) {
    const cmd = `claude --resume ${sess.session_id}`;
    try {
      await navigator.clipboard.writeText(cmd);
      copiedId = sess.session_id;
      setTimeout(() => {
        if (copiedId === sess.session_id) copiedId = null;
      }, 1800);
    } catch {
      showToast("err", "Clipboard access failed");
    }
  }

  function formatDate(iso: string | null): string {
    if (!iso) return "—";
    try {
      return new Date(iso).toLocaleString();
    } catch {
      return iso;
    }
  }

  function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  }

  let filteredSessions = $derived(
    sessions.filter((s) => {
      const q = searchQuery.trim().toLowerCase();
      if (!q) return true;
      return (
        s.title.toLowerCase().includes(q) ||
        s.project_name.toLowerCase().includes(q) ||
        s.session_id.toLowerCase().includes(q)
      );
    })
  );

  onMount(() => {
    refreshAll();
    const unlistenStart = listen("sync-started", () => {
      syncing = true;
    });
    const unlistenDone = listen<SyncReport>("sync-completed", () => {
      syncing = false;
      refreshAll();
    });
    const unlistenErr = listen<string>("sync-error", (ev) => {
      syncing = false;
      showToast("err", ev.payload);
    });

    return () => {
      unlistenStart.then((fn) => fn());
      unlistenDone.then((fn) => fn());
      unlistenErr.then((fn) => fn());
    };
  });
</script>

<div class="shell">
  <header class="topbar">
    <div class="brand">
      <div class="logo-dot"></div>
      <div>
        <h1>SessionShare</h1>
        <span class="subtitle">
          {status?.machine_name ?? "Windows PC"} · Local Synced Drive
        </span>
      </div>
    </div>

    <div class="topbar-actions">
      {#if status?.configured}
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

  <nav class="tabs">
    <button class:active={activeTab === "projects"} onclick={() => (activeTab = "projects")}>
      Projects ({projects.length})
    </button>
    <button class:active={activeTab === "sessions"} onclick={() => (activeTab = "sessions")}>
      Sessions ({sessions.length})
    </button>
    <button class:active={activeTab === "global"} onclick={() => (activeTab = "global")}>
      Global Assets
    </button>
    <button class:active={activeTab === "activity"} onclick={() => (activeTab = "activity")}>
      Activity ({activities.length})
    </button>
    <button class:active={activeTab === "settings"} onclick={() => (activeTab = "settings")}>
      Settings
    </button>
  </nav>

  {#if toast}
    <div class="toast" class:err={toast.kind === "err"}>{toast.text}</div>
  {/if}

  <main class="content">
    {#if !config?.sync_dir && activeTab !== "settings"}
      <div class="empty-card">
        <h2>Choose your Synced Drive folder</h2>
        <p>
          Point SessionShare to a folder inside your approved OneDrive / Google Drive / Dropbox
          directory on both laptops. SessionShare makes zero outbound network calls itself.
        </p>
        <button class="btn primary" onclick={() => (activeTab = "settings")}>
          Open Setup
        </button>
      </div>
    {:else if activeTab === "projects"}
      <div class="section-header">
        <div>
          <h2>Projects</h2>
          <p>
            Projects with the same Git remote auto-link across PCs. Toggle sync per project or map
            unlinked remote projects to a local folder.
          </p>
        </div>
        <button class="btn" onclick={refreshAll}>Refresh</button>
      </div>

      {#if projects.length === 0}
        <div class="empty-card">
          <p>No Claude Code projects found yet. Run Claude Code in a folder or sync from your other PC.</p>
        </div>
      {:else}
        <div class="card-list">
          {#each projects as proj}
            <div class="card project-card">
              <div class="project-main">
                <div class="project-title-row">
                  <strong>{proj.display_name}</strong>
                  {#if proj.needs_mapping}
                    <span class="badge warn">Needs Local Mapping</span>
                  {:else if proj.sync_enabled}
                    <span class="badge ok">Synced</span>
                  {:else}
                    <span class="badge muted">Local Only</span>
                  {/if}
                  {#if proj.mapping_source && formatMatchSource(proj.mapping_source)}
                    <span class="badge">{formatMatchSource(proj.mapping_source)}</span>
                  {/if}
                  {#if proj.remote_machines.length > 0}
                    <span class="badge info">Also on: {proj.remote_machines.join(", ")}</span>
                  {/if}
                </div>
                <div class="meta-line mono">
                  {proj.local_path ?? "Not mapped on this PC yet"}
                </div>
                {#if proj.git_remote}
                  <div class="meta-line">Git: <span class="mono">{proj.git_remote}</span></div>
                {/if}
                <div class="meta-line">
                  Local sessions: {proj.local_session_count} · Synced drive sessions: {proj.remote_session_count}
                </div>
              </div>
              <div class="project-actions">
                <button class="btn" onclick={() => handleMapProject(proj)}>
                  {proj.local_path ? "Change Folder" : "Map Local Folder"}
                </button>
                <label class="switch">
                  <input
                    type="checkbox"
                    checked={proj.sync_enabled}
                    disabled={proj.needs_mapping}
                    onchange={() => handleToggleProject(proj)}
                  />
                  <span>Sync</span>
                </label>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    {:else if activeTab === "sessions"}
      <div class="section-header">
        <div>
          <h2>Sessions</h2>
          <p>Search conversations across both PCs and copy the resume command.</p>
        </div>
        <input
          class="search-input"
          type="text"
          placeholder="Search sessions or projects..."
          bind:value={searchQuery}
        />
      </div>

      {#if filteredSessions.length === 0}
        <div class="empty-card">
          <p>No sessions match your filter.</p>
        </div>
      {:else}
        <div class="card-list">
          {#each filteredSessions as sess}
            <div class="card session-card">
              <div class="session-main">
                <div class="project-title-row">
                  <strong>{sess.title}</strong>
                  <span class="badge info">{sess.project_name}</span>
                  {#if sess.in_local && sess.in_remote}
                    <span class="badge ok">Local + Drive</span>
                  {:else if sess.in_local}
                    <span class="badge muted">Local Only</span>
                  {:else}
                    <span class="badge warn">Drive Only</span>
                  {/if}
                </div>
                <div class="meta-line">
                  <span class="mono">{sess.session_id}</span> · {sess.line_count} events ·
                  {formatBytes(sess.size_bytes)} · Updated {formatDate(sess.last_modified)}
                </div>
              </div>
              <button class="btn" onclick={() => copyResumeCommand(sess)}>
                {copiedId === sess.session_id ? "Copied!" : "Copy --resume"}
              </button>
            </div>
          {/each}
        </div>
      {/if}
    {:else if activeTab === "global" && config}
      <div class="section-header">
        <div>
          <h2>Global Claude Code Assets</h2>
          <p>Choose which portable Claude Code folders and files sync across your PCs.</p>
        </div>
      </div>

      <div class="card-list">
        {#each [
          ["project_memory", "Project Memory (memory/)", "Per-project Claude auto-memory files"],
          ["skills", "Skills (skills/)", "Custom reusable Claude Code skills"],
          ["agents", "Agents (agents/)", "Custom subagent definitions"],
          ["commands", "Slash Commands (commands/)", "Custom slash command prompts"],
          ["plans", "Plans (plans/)", "Saved Claude Code plan documents"],
          ["rules", "Rules (rules/)", "Custom Claude Code rule files"],
          ["hooks", "Hooks (hooks/)", "Hook scripts (referenced paths auto-remapped in settings.json)"],
          ["claude_md", "Global CLAUDE.md", "Global instructions across all projects"],
          ["settings_json", "settings.json", "Claude Code settings with automatic .claude/home path remapping"],
          ["history_jsonl", "history.jsonl", "Global prompt history with automatic project path remapping"],
        ] as [key, label, desc]}
          <div class="card global-row">
            <div>
              <strong>{label}</strong>
              <div class="meta-line">{desc}</div>
            </div>
            <label class="switch">
              <input
                type="checkbox"
                checked={config.global_options[key as keyof AppConfig["global_options"]]}
                onchange={() => handleToggleGlobal(key as keyof AppConfig["global_options"])}
              />
              <span>Enabled</span>
            </label>
          </div>
        {/each}
      </div>
    {:else if activeTab === "activity"}
      <div class="section-header">
        <div>
          <h2>Sync Activity</h2>
          <p>Recent pushes, pulls, merges, and conflict backups.</p>
        </div>
        <button class="btn" onclick={refreshAll}>Refresh</button>
      </div>

      {#if activities.length === 0}
        <div class="empty-card"><p>No sync activity recorded yet.</p></div>
      {:else}
        <div class="card-list">
          {#each activities as act}
            <div class="card activity-row">
              <span class="badge" class:ok={act.kind === "push" || act.kind === "pull"} class:info={act.kind === "merge"} class:warn={act.kind === "conflict"}>
                {act.kind.toUpperCase()}
              </span>
              <div class="activity-body">
                <div class="mono">{act.path}</div>
                <div class="meta-line">{act.detail} · {formatDate(act.timestamp)}</div>
              </div>
            </div>
          {/each}
        </div>
      {/if}
    {:else if activeTab === "settings"}
      <div class="section-header">
        <div>
          <h2>Settings</h2>
          <p>Configure this PC's identity, local Claude folder, and approved synced drive folder.</p>
        </div>
      </div>

      <div class="card form-card">
        <label class="field">
          <span>Machine Name</span>
          <input type="text" bind:value={machineName} placeholder="e.g. Personal-PC or Office-Laptop" />
        </label>

        <label class="field">
          <span>Approved Synced Drive Folder</span>
          <div class="input-row">
            <input
              type="text"
              class="mono"
              bind:value={syncDir}
              placeholder="C:\Users\You\OneDrive\SessionShare"
            />
            <button class="btn" onclick={handlePickSyncDir}>Browse...</button>
          </div>
        </label>

        <label class="field">
          <span>Local Claude Code Directory</span>
          <div class="input-row">
            <input type="text" class="mono" bind:value={claudeDir} />
            <button class="btn" onclick={handlePickClaudeDir}>Browse...</button>
          </div>
        </label>

        <div class="field">
          <span>Workspace Search Roots (for Smart Folder Mismatch Auto-Discovery)</span>
          <div class="meta-line">
            Add parent dev folders on this PC (e.g. <span class="mono">C:\Dev</span>,
            <span class="mono">D:\Work\Repos</span>) so SessionShare automatically links projects
            with different paths or folder names across computers.
          </div>
          <div class="roots-list">
            {#each workspaceRoots as root}
              <div class="root-chip">
                <span class="mono">{root}</span>
                <button class="chip-x" onclick={() => handleRemoveWorkspaceRoot(root)}>×</button>
              </div>
            {/each}
            <button class="btn" onclick={handleAddWorkspaceRoot}>+ Add Workspace Root</button>
          </div>
        </div>

        <div class="inline-fields">
          <label class="switch">
            <input type="checkbox" bind:checked={autoSync} />
            <span>Watch folders and auto-sync in background</span>
          </label>
          <label class="field small">
            <span>Debounce (seconds)</span>
            <input type="number" min="2" max="60" bind:value={debounceSecs} />
          </label>
        </div>

        <div class="form-footer">
          <button class="btn primary" onclick={handleSaveSettings}>Save Settings</button>
        </div>
      </div>
    {/if}
  </main>
</div>

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  .topbar {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 14px 20px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .logo-dot {
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: linear-gradient(135deg, #22d3ee, #6366f1);
    box-shadow: 0 0 12px rgba(56, 189, 248, 0.5);
  }

  .brand h1 {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
  }

  .subtitle {
    font-size: 12px;
    color: var(--muted);
  }

  .topbar-actions {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  .tabs {
    display: flex;
    gap: 4px;
    padding: 8px 20px 0;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }

  .tabs button {
    background: transparent;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--muted);
    padding: 8px 14px;
    font-size: 13px;
    font-weight: 500;
  }

  .tabs button.active {
    color: var(--text);
    border-bottom-color: var(--accent);
  }

  .content {
    flex: 1;
    overflow-y: auto;
    padding: 20px;
  }

  .section-header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: 16px;
    margin-bottom: 16px;
  }

  .section-header h2 {
    margin: 0 0 4px;
    font-size: 18px;
  }

  .section-header p {
    margin: 0;
    color: var(--muted);
    font-size: 13px;
  }

  .card-list {
    display: flex;
    flex-direction: column;
    gap: 10px;
  }

  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 10px;
    padding: 14px 16px;
  }

  .empty-card {
    background: var(--surface);
    border: 1px dashed var(--border);
    border-radius: 10px;
    padding: 28px;
    text-align: center;
    color: var(--muted);
  }

  .project-card,
  .session-card,
  .global-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
  }

  .project-title-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 4px;
  }

  .meta-line {
    font-size: 12px;
    color: var(--muted);
    margin-top: 2px;
  }

  .project-actions {
    display: flex;
    align-items: center;
    gap: 12px;
    flex-shrink: 0;
  }

  .activity-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .btn {
    background: var(--surface-hover);
    color: var(--text);
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 7px 12px;
    font-size: 13px;
    font-weight: 500;
  }

  .btn:hover:not(:disabled) {
    border-color: var(--accent);
  }

  .btn.primary {
    background: var(--accent-strong);
    border-color: var(--accent-strong);
    color: #fff;
  }

  .btn:disabled {
    opacity: 0.6;
    cursor: default;
  }

  .pill,
  .badge {
    font-size: 11px;
    font-weight: 600;
    padding: 2px 8px;
    border-radius: 999px;
    background: #232837;
    color: var(--muted);
  }

  .pill.active,
  .badge.ok {
    background: rgba(52, 211, 153, 0.15);
    color: var(--success);
  }

  .badge.info {
    background: rgba(56, 189, 248, 0.15);
    color: var(--accent);
  }

  .pill.warn,
  .badge.warn {
    background: rgba(251, 191, 36, 0.15);
    color: var(--warning);
  }

  .switch {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    cursor: pointer;
  }

  .search-input,
  .field input {
    background: var(--bg);
    border: 1px solid var(--border);
    color: var(--text);
    border-radius: 7px;
    padding: 8px 10px;
    font-size: 13px;
    width: 100%;
  }

  .search-input {
    width: 260px;
  }

  .form-card {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 680px;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .field span {
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }

  .field.small {
    width: 140px;
  }

  .input-row {
    display: flex;
    gap: 8px;
  }

  .inline-fields {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 16px;
  }

  .form-footer {
    display: flex;
    justify-content: flex-end;
  }

  .roots-list {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    margin-top: 6px;
  }

  .root-chip {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 7px;
    padding: 5px 9px;
    font-size: 12px;
  }

  .chip-x {
    background: transparent;
    border: none;
    color: var(--muted);
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
  }

  .chip-x:hover {
    color: var(--danger);
  }

  .toast {
    margin: 10px 20px 0;
    padding: 9px 14px;
    border-radius: 8px;
    font-size: 13px;
    background: rgba(52, 211, 153, 0.15);
    border: 1px solid rgba(52, 211, 153, 0.35);
    color: var(--success);
  }

  .toast.err {
    background: rgba(248, 113, 113, 0.15);
    border-color: rgba(248, 113, 113, 0.35);
    color: var(--danger);
  }
</style>

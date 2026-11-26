<script lang="ts">
  import { untrack } from "svelte";
  import { api, settingsPayload } from "../lib/api";
  import type { AppConfig, SettingsPayload } from "../lib/types";

  interface Props {
    config: AppConfig;
    onSave: (payload: SettingsPayload) => Promise<void>;
  }

  let { config, onSave }: Props = $props();

  // Form state is captured when the tab opens, so background syncs never overwrite edits in progress.
  const initial = untrack(() => settingsPayload(config));
  let machineName = $state(initial.machine_name);
  let claudeDir = $state(initial.claude_dir);
  let claudeDesktopDir = $state(initial.claude_desktop_dir ?? "");
  let syncDir = $state(initial.sync_dir ?? "");
  let autoSync = $state(initial.auto_sync);
  let debounceSecs = $state(initial.debounce_secs);
  let periodicMins = $state(initial.periodic_sync_mins);
  let graceSecs = $state(initial.active_session_grace_secs);
  let workspaceRoots = $state<string[]>([...initial.workspace_roots]);
  let propagateDeletions = $state(initial.propagate_deletions);
  let requireHookApproval = $state(initial.require_hook_approval);
  let excludedKeys = $state(initial.settings_excluded_keys.join(", "));
  let retentionDays = $state(initial.retention_days);
  let launchAtLogin = $state(initial.launch_at_login);
  let saving = $state(false);

  function toInt(v: unknown, fallback: number, min = 0): number {
    const n = Math.floor(Number(v));
    return Number.isFinite(n) ? Math.max(min, n) : fallback;
  }

  async function pickSyncDir() {
    const picked = await api.pickFolder(
      "Select your approved Synced Drive folder (e.g. OneDrive\\SessionShare)"
    );
    if (picked) syncDir = picked;
  }

  async function pickClaudeDir() {
    const picked = await api.pickFolder("Select your local .claude folder");
    if (picked) claudeDir = picked;
  }

  async function pickClaudeDesktopDir() {
    const picked = await api.pickFolder(
      "Select Claude Desktop's claude-code-sessions folder (or leave blank to auto-detect)"
    );
    if (picked) claudeDesktopDir = picked;
  }

  async function addWorkspaceRoot() {
    const picked = await api.pickFolder("Select a workspace root folder (e.g. C:\\Dev or D:\\Work)");
    if (picked && !workspaceRoots.some((r) => r.toLowerCase() === picked.toLowerCase())) {
      workspaceRoots = [...workspaceRoots, picked];
    }
  }

  async function save() {
    saving = true;
    try {
      await onSave(
        settingsPayload(config, {
          machine_name: machineName,
          claude_dir: claudeDir,
          claude_desktop_dir: claudeDesktopDir.trim() || null,
          sync_dir: syncDir.trim() || null,
          auto_sync: autoSync,
          debounce_secs: toInt(debounceSecs, 6, 2),
          periodic_sync_mins: toInt(periodicMins, 5),
          active_session_grace_secs: toInt(graceSecs, 30),
          workspace_roots: workspaceRoots,
          propagate_deletions: propagateDeletions,
          require_hook_approval: requireHookApproval,
          settings_excluded_keys: excludedKeys
            .split(",")
            .map((k) => k.trim())
            .filter(Boolean),
          retention_days: toInt(retentionDays, 30, 1),
          launch_at_login: launchAtLogin,
        })
      );
    } finally {
      saving = false;
    }
  }
</script>

<div class="section-header">
  <div>
    <h2>Settings</h2>
    <p>Configure this PC's identity, local Claude folder, and approved synced drive folder.</p>
  </div>
</div>

<div class="card form">
  <label class="field">
    <span>Machine Name</span>
    <input class="text-input" type="text" bind:value={machineName} placeholder="e.g. Personal-PC or Office-Laptop" />
  </label>

  <label class="field">
    <span>Approved Synced Drive Folder</span>
    <div class="input-row">
      <input
        type="text"
        class="text-input mono"
        bind:value={syncDir}
        placeholder="C:\Users\You\OneDrive\SessionShare"
      />
      <button class="btn" onclick={pickSyncDir}>Browse...</button>
    </div>
  </label>

  <label class="field">
    <span>Local Claude Code Directory</span>
    <div class="input-row">
      <input type="text" class="text-input mono" bind:value={claudeDir} />
      <button class="btn" onclick={pickClaudeDir}>Browse...</button>
    </div>
  </label>

  <label class="field">
    <span>Claude Desktop Sessions Directory (optional — leave blank to auto-detect)</span>
    <div class="input-row">
      <input
        type="text"
        class="text-input mono"
        bind:value={claudeDesktopDir}
        placeholder="Auto-detected from %LOCALAPPDATA%\Claude-Data or %APPDATA%\Claude"
      />
      <button class="btn" onclick={pickClaudeDesktopDir}>Browse...</button>
    </div>
  </label>

  <div class="field">
    <span>Workspace Search Roots (for Smart Folder Mismatch Auto-Discovery)</span>
    <div class="meta-line">
      Add parent dev folders on this PC (e.g. <span class="mono">C:\Dev</span>,
      <span class="mono">D:\Work\Repos</span>) so SessionShare automatically links projects with
      different paths or folder names across computers.
    </div>
    <div class="roots">
      {#each workspaceRoots as root (root)}
        <div class="chip">
          <span class="mono">{root}</span>
          <button
            class="chip-x"
            aria-label="Remove {root}"
            onclick={() => (workspaceRoots = workspaceRoots.filter((r) => r !== root))}>×</button
          >
        </div>
      {/each}
      <button class="btn" onclick={addWorkspaceRoot}>+ Add Workspace Root</button>
    </div>
  </div>

  <h3>Background sync</h3>
  <label class="switch">
    <input type="checkbox" bind:checked={autoSync} />
    <span>Watch folders and auto-sync in background</span>
  </label>
  <label class="switch">
    <input type="checkbox" bind:checked={launchAtLogin} />
    <span>Start SessionShare in the tray when I sign in to Windows</span>
  </label>
  <div class="grid">
    <label class="field">
      <span>Debounce (seconds)</span>
      <input class="text-input" type="number" min="2" max="60" bind:value={debounceSecs} />
    </label>
    <label class="field">
      <span>Periodic full sync (minutes, 0 = off)</span>
      <input class="text-input" type="number" min="0" max="1440" bind:value={periodicMins} />
    </label>
    <label class="field">
      <span>Active session grace (seconds)</span>
      <input class="text-input" type="number" min="0" max="3600" bind:value={graceSecs} />
    </label>
  </div>
  <div class="meta-line">
    Transcripts changed within the grace period are treated as in use: new lines from the other
    PC are applied once the session goes idle, so a running Claude session is never overwritten.
  </div>

  <h3>Safety</h3>
  <label class="switch">
    <input type="checkbox" bind:checked={propagateDeletions} />
    <span>Sync deletions (deleted files go to a trash folder, not erased)</span>
  </label>
  <label class="switch">
    <input type="checkbox" bind:checked={requireHookApproval} />
    <span>Ask before applying incoming hooks, permissions or command helpers</span>
  </label>
  <div class="grid">
    <label class="field wide">
      <span>settings.json keys kept on this PC only (comma-separated)</span>
      <input class="text-input mono" type="text" bind:value={excludedKeys} placeholder="env" />
    </label>
    <label class="field">
      <span>Keep trash &amp; conflict backups (days)</span>
      <input class="text-input" type="number" min="1" max="365" bind:value={retentionDays} />
    </label>
  </div>

  <div class="footer">
    <button class="btn primary" onclick={save} disabled={saving}>
      {saving ? "Saving..." : "Save Settings"}
    </button>
  </div>
</div>

<style>
  .form {
    display: flex;
    flex-direction: column;
    gap: 14px;
    max-width: 720px;
  }

  h3 {
    margin: 8px 0 0;
    font-size: 13px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .field > span {
    font-size: 12px;
    font-weight: 600;
    color: var(--muted);
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 12px;
  }

  .grid .wide {
    grid-column: span 2;
  }

  .input-row {
    display: flex;
    gap: 8px;
  }

  .footer {
    display: flex;
    justify-content: flex-end;
  }

  .roots {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    margin-top: 6px;
  }

  .chip {
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
</style>
